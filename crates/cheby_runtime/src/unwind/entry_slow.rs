//! The entry check's releasing slow paths (D-775, refining D-698 and
//! D-743).
//!
//! Every Cheby function starts with the stack check of D-124. Its slow
//! path may start an unwinding (a stack overflow, a cancellation, the
//! constant-evaluation deadline), and the unwinding must then release the
//! function's counted parameters, which the MIR's entry pad drops (D-698,
//! D-752). For a function with at most four counted parameters, code
//! generation does not translate that entry-only pad chain: the slow block
//! calls one of the helpers here with the parameters instead, and then
//! branches on the fiber's unwind flag to the function's shared `resume`
//! (set) or to the body (clear). The MIR keeps its entry pad; a function
//! with more counted parameters keeps the translated chain.
//!
//! Like the plain slow paths, the helpers save every register generated
//! code may use and load them back before they return
//! (`crate::panic::save_all_aarch64`, `crate::panic::save_all_x86_64`), so
//! their call clobbers nothing.
//! They return no result: Cranelift's `preserve_all` convention, which
//! generated code calls them with, has none, and the flag tells the same.
//!
//! The helpers are not guarded themselves. Each calls the guarded slow
//! path (`cheby_rt_stack_slow` or `cheby_rt_stack_slow_frame`), which
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
use crate::backtrace::{Start, start_at};
use crate::ctx::ExecCtx;
use crate::panic::{fatal, stack_slow_at, stack_slow_frame_at};
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
use crate::panic::{restore_all_aarch64, save_all_aarch64};
#[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
use crate::panic::{restore_all_x86_64, save_all_x86_64};
use crate::rc;

/// The entry check's slow path for a function with at most four counted
/// parameters (D-775). Runs [`crate::panic::cheby_rt_stack_slow`]'s work;
/// if that started an unwinding (the flag is set: overflow, cancellation,
/// deadline), releases `p0..p(count-1)`; else leaves them untouched. The
/// caller reads the flag to tell. Never raises itself.
///
/// D-1040 (D-1236): generated code reaches the
/// slow path through this helper too, so it is a naked stub as well. Its
/// arguments fill `x0`-`x6` (AAPCS64), so it pushes a frame record of its
/// own (`x29`, `x30`), saves every register generated code may use below
/// it ([`save_all_aarch64`]), passes the record's address in `x7` and
/// calls [`stack_slow_unwind_from`], which reads the caller's frame
/// pointer and return address from it; then it loads the registers back,
/// pops the record and returns.
///
/// # Safety
///
/// As for [`crate::panic::cheby_rt_stack_slow`]; `count` is at most
/// [`ENTRY_UNWIND_WORDS`](crate::abi::ENTRY_UNWIND_WORDS), and
/// `p0..p(count-1)` are each 0, a tagged integer or a reference the caller
/// owns. The words from `count` on are ignored.
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_rt_stack_slow_unwind(
    ctx: *mut ExecCtx,
    location: Word,
    count: u32,
    p0: Word,
    p1: Word,
    p2: Word,
    p3: Word,
) {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-16]!",
        "mov x29, sp",
        save_all_aarch64!(),
        "mov x7, x29",
        "bl {body}",
        restore_all_aarch64!(),
        "ldp x29, x30, [sp], #16",
        "ret",
        body = sym stack_slow_unwind_from,
    )
}

/// The releasing slow path (above), as a System V x86-64 stub (D-1040): its
/// arguments fill the six registers and one stack word, so it pushes
/// `rbp` as a frame record of its own, saves every register generated code
/// may use below it ([`save_all_x86_64`]), then calls
/// [`stack_slow_unwind_from`] with the seventh argument, `p3`, copied
/// from `[rbp + 16]` and the record's address as the eighth, both on the
/// stack, the call 16-byte aligned; then it drops the two words, loads the
/// registers back and pops the record.
///
/// # Safety
///
/// As for the AAPCS64 stub.
#[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_rt_stack_slow_unwind(
    ctx: *mut ExecCtx,
    location: Word,
    count: u32,
    p0: Word,
    p1: Word,
    p2: Word,
    p3: Word,
) {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        save_all_x86_64!(),
        "push rbp",
        "push qword ptr [rbp + 16]",
        "call {body}",
        "add rsp, 16",
        restore_all_x86_64!(),
        "pop rbp",
        "ret",
        body = sym stack_slow_unwind_from,
    )
}

/// The releasing slow path (above), a plain function under Miri and on
/// Windows until M6 (D-1040).
///
/// # Safety
///
/// As above.
#[cfg(not(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    not(miri),
    not(windows)
)))]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_rt_stack_slow_unwind(
    ctx: *mut ExecCtx,
    location: Word,
    count: u32,
    p0: Word,
    p1: Word,
    p2: Word,
    p3: Word,
) {
    // SAFETY: forwarded from the caller; 0 is no frame record.
    unsafe { stack_slow_unwind_from(ctx, location, count, p0, p1, p2, p3, 0) }
}

/// The body of [`cheby_rt_stack_slow_unwind`]: `record` is the address of
/// the stub's own frame record, which holds the generated caller's frame
/// pointer and return address, or 0 (D-1040).
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow_unwind`]; `record` is 0 or the address of
/// two readable words on the running fiber's stack.
unsafe extern "C" fn stack_slow_unwind_from(
    ctx: *mut ExecCtx,
    location: Word,
    count: u32,
    p0: Word,
    p1: Word,
    p2: Word,
    p3: Word,
    record: usize,
) {
    // SAFETY: forwarded from the caller.
    let start = unsafe { start_of(record) };
    // SAFETY: forwarded from the caller.
    unsafe { stack_slow_at(ctx, location, start) };
    // SAFETY: forwarded from the caller.
    unsafe { release_if_unwinding(ctx, count, [p0, p1, p2, p3]) }
}

/// [`cheby_rt_stack_slow_unwind`] for the outer function of a large frame
/// of `frame_size` bytes, through [`crate::panic::cheby_rt_stack_slow_frame`]'s
/// work (D-743, D-775).
///
/// D-1040 (D-1236): a naked stub, as
/// [`cheby_rt_stack_slow_unwind`]'s. Its arguments fill `x0`-`x7`
/// (AAPCS64), so it pushes its frame record (`x29`, `x30`), saves every
/// register generated code may use below it, makes 16 bytes of room for
/// the ninth argument, the record's address, at `[sp]`, and calls
/// [`stack_slow_frame_unwind_from`].
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow_unwind`].
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_rt_stack_slow_frame_unwind(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
    count: u32,
    p0: Word,
    p1: Word,
    p2: Word,
    p3: Word,
) {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-16]!",
        "mov x29, sp",
        save_all_aarch64!(),
        "sub sp, sp, #16",
        "str x29, [sp]",
        "bl {body}",
        "add sp, sp, #16",
        restore_all_aarch64!(),
        "ldp x29, x30, [sp], #16",
        "ret",
        body = sym stack_slow_frame_unwind_from,
    )
}

/// A large function's releasing slow path (above), as a System V x86-64 stub
/// (D-1040): its arguments fill the six registers and two stack words, so
/// it pushes `rbp` as a frame record of its own, saves every register
/// generated code may use below it, then calls
/// [`stack_slow_frame_unwind_from`] with `p2` and `p3` copied from
/// `[rbp + 16]` and `[rbp + 24]` and the record's address as the ninth
/// argument, the call 16-byte aligned by an 8-byte pad; then it drops the
/// four words, loads the registers back and pops the record.
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow_unwind`].
#[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_rt_stack_slow_frame_unwind(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
    count: u32,
    p0: Word,
    p1: Word,
    p2: Word,
    p3: Word,
) {
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        save_all_x86_64!(),
        "sub rsp, 8",
        "push rbp",
        "push qword ptr [rbp + 24]",
        "push qword ptr [rbp + 16]",
        "call {body}",
        "add rsp, 32",
        restore_all_x86_64!(),
        "pop rbp",
        "ret",
        body = sym stack_slow_frame_unwind_from,
    )
}

/// A large function's releasing slow path (above), a plain function under
/// Miri and on Windows until M6 (D-1040).
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow_unwind`].
#[cfg(not(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    not(miri),
    not(windows)
)))]
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn cheby_rt_stack_slow_frame_unwind(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
    count: u32,
    p0: Word,
    p1: Word,
    p2: Word,
    p3: Word,
) {
    // SAFETY: forwarded from the caller; 0 is no frame record.
    unsafe { stack_slow_frame_unwind_from(ctx, location, frame_size, count, p0, p1, p2, p3, 0) }
}

/// The body of [`cheby_rt_stack_slow_frame_unwind`]: `record` as for
/// [`stack_slow_unwind_from`].
///
/// # Safety
///
/// As for [`stack_slow_unwind_from`].
unsafe extern "C" fn stack_slow_frame_unwind_from(
    ctx: *mut ExecCtx,
    location: Word,
    frame_size: u64,
    count: u32,
    p0: Word,
    p1: Word,
    p2: Word,
    p3: Word,
    record: usize,
) {
    // SAFETY: forwarded from the caller.
    let start = unsafe { start_of(record) };
    // SAFETY: forwarded from the caller.
    unsafe { stack_slow_frame_at(ctx, location, frame_size, start) };
    // SAFETY: forwarded from the caller.
    unsafe { release_if_unwinding(ctx, count, [p0, p1, p2, p3]) }
}

/// The start a releasing stub's frame record at `record` names: the
/// caller's frame pointer at `record` and its return address at
/// `record + 8`, or no start for 0 (D-1040).
///
/// # Safety
///
/// `record` is 0 or the address of two readable, aligned words.
unsafe fn start_of(record: usize) -> Start {
    if record == 0 {
        return Start::Unknown;
    }
    let words = std::ptr::with_exposed_provenance::<[usize; 2]>(record);
    // SAFETY: the caller guarantees two readable, aligned words at
    // `record`: the stub's frame record on the running fiber's stack, which
    // the stub keeps until this body returns.
    let [fp, ra] = unsafe { words.read() };
    start_at(fp, ra)
}

/// After a slow path returned: when the flag of `ctx` shows an unwinding,
/// releases the first `count` of `words`; otherwise releases nothing.
///
/// # Safety
///
/// As for [`cheby_rt_stack_slow_unwind`]; the slow path has run on `ctx`.
unsafe fn release_if_unwinding(ctx: *mut ExecCtx, count: u32, words: [Word; 4]) {
    // SAFETY: forwarded from the caller; the slow path found the context,
    // so it is the running fiber's live control block.
    let Some(fiber) = (unsafe { ctx.as_ref() }) else {
        fatal(format_args!(
            "a stack check ran without an execution context"
        ));
    };
    if super::flag::get(fiber) == unwind_flag::NONE {
        return;
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
}

#[cfg(test)]
pub(crate) mod tests {
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

    /// Calls `helper` on the running fiber `ctx` with `words`, and returns
    /// 1 when it started an unwinding (the flag is set), as generated code
    /// tells it, and 0 otherwise.
    fn call(helper: Helper, ctx: &ExecCtx, words: &[Word]) -> u32 {
        let mut p = [0; 4];
        p[..words.len()].copy_from_slice(words);
        let count = u32::try_from(words.len()).unwrap();
        let fiber = ctx;
        let ctx = ptr::from_ref(ctx).cast_mut();
        match helper {
            // SAFETY: the running fiber's context, no location, and words
            // the fiber owns.
            Helper::Plain => unsafe {
                cheby_rt_stack_slow_unwind(ctx, 0, count, p[0], p[1], p[2], p[3]);
            },
            // SAFETY: as above.
            Helper::Frame(size) => unsafe {
                cheby_rt_stack_slow_frame_unwind(ctx, 0, size, count, p[0], p[1], p[2], p[3]);
            },
        }
        u32::from(crate::unwind::flag::get(fiber) != unwind_flag::NONE)
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

    /// Every register a slow path's stub saves, in the order the test's
    /// assembly loads and stores them. On aarch64
    /// (`crate::panic::save_all_aarch64`) `x` is `x0`-`x17` and `q` is
    /// `q0`-`q31` as pairs of words; on x86-64
    /// (`crate::panic::save_all_x86_64`) `x` is `rax`, `rcx`, `rdx`,
    /// `rsi`, `rdi` and `r8`-`r11`, and `q` is `xmm0`-`xmm15` as pairs of
    /// words.
    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        not(miri),
        not(windows)
    ))]
    #[repr(C, align(16))]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct Regs {
        pub(crate) x: [u64; GENERAL],
        pub(crate) q: [u64; VECTOR_WORDS],
    }

    /// The number of general registers in [`Regs`].
    #[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
    const GENERAL: usize = 18;

    /// The number of words of vector registers in [`Regs`].
    #[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
    const VECTOR_WORDS: usize = 64;

    /// Where in [`Regs::x`] the C convention's integer arguments go:
    /// `x0`-`x7`.
    #[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
    const ARGUMENTS: [usize; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

    /// The number of general registers in [`Regs`].
    #[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
    const GENERAL: usize = 9;

    /// The number of words of vector registers in [`Regs`].
    #[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
    const VECTOR_WORDS: usize = 32;

    /// Where in [`Regs::x`] the C convention's integer arguments go:
    /// `rdi`, `rsi`, `rdx`, `rcx`, `r8` and `r9`.
    #[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
    const ARGUMENTS: [usize; 6] = [4, 3, 2, 1, 5, 6];

    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        not(miri),
        not(windows)
    ))]
    impl Regs {
        /// Every word a distinct pattern.
        pub(crate) fn patterned() -> Self {
            Self {
                x: std::array::from_fn(|i| 0x1111_2222_0000_0000 | u64::try_from(i).unwrap()),
                q: std::array::from_fn(|i| 0x5a5a_a5a5_0000_0000 | u64::try_from(i).unwrap()),
            }
        }

        /// Puts `value` where the C convention passes the integer argument
        /// `index` in a register.
        pub(crate) fn set_argument(&mut self, index: usize, value: u64) {
            self.x[ARGUMENTS[index]] = value;
        }
    }

    /// Calls the stub at `stub` with `x0`-`x17` and `q0`-`q31` loaded from
    /// `input`, and returns them as the stub left them.
    ///
    /// # Safety
    ///
    /// `stub` is one of the slow paths' stubs, and the arguments `input`
    /// passes in registers are ones it accepts.
    #[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
    pub(crate) unsafe fn call_saving(stub: usize, input: &Regs) -> Regs {
        let mut output = Regs {
            x: [0; GENERAL],
            q: [0; VECTOR_WORDS],
        };
        let from = ptr::from_ref(input);
        let to = ptr::from_mut(&mut output);
        // SAFETY: loads every register from `input`, calls the stub, which
        // the caller guarantees, and stores every register into `output`;
        // both live through the block. Everything the block writes is an
        // output or among the C convention's clobbers.
        unsafe {
            core::arch::asm!(
            "ldp q0, q1, [x20, #144]",
            "ldp q2, q3, [x20, #176]",
            "ldp q4, q5, [x20, #208]",
            "ldp q6, q7, [x20, #240]",
            "ldp q8, q9, [x20, #272]",
            "ldp q10, q11, [x20, #304]",
            "ldp q12, q13, [x20, #336]",
            "ldp q14, q15, [x20, #368]",
            "ldp q16, q17, [x20, #400]",
            "ldp q18, q19, [x20, #432]",
            "ldp q20, q21, [x20, #464]",
            "ldp q22, q23, [x20, #496]",
            "ldp q24, q25, [x20, #528]",
            "ldp q26, q27, [x20, #560]",
            "ldp q28, q29, [x20, #592]",
            "ldp q30, q31, [x20, #624]",
            "ldp x2, x3, [x20, #16]",
            "ldp x4, x5, [x20, #32]",
            "ldp x6, x7, [x20, #48]",
            "ldp x8, x9, [x20, #64]",
            "ldp x10, x11, [x20, #80]",
            "ldp x12, x13, [x20, #96]",
            "ldp x14, x15, [x20, #112]",
            "ldp x16, x17, [x20, #128]",
            "ldp x0, x1, [x20, #0]",
            "blr x22",
            "stp x0, x1, [x21, #0]",
            "stp x2, x3, [x21, #16]",
            "stp x4, x5, [x21, #32]",
            "stp x6, x7, [x21, #48]",
            "stp x8, x9, [x21, #64]",
            "stp x10, x11, [x21, #80]",
            "stp x12, x13, [x21, #96]",
            "stp x14, x15, [x21, #112]",
            "stp x16, x17, [x21, #128]",
            "stp q0, q1, [x21, #144]",
            "stp q2, q3, [x21, #176]",
            "stp q4, q5, [x21, #208]",
            "stp q6, q7, [x21, #240]",
            "stp q8, q9, [x21, #272]",
            "stp q10, q11, [x21, #304]",
            "stp q12, q13, [x21, #336]",
            "stp q14, q15, [x21, #368]",
            "stp q16, q17, [x21, #400]",
            "stp q18, q19, [x21, #432]",
            "stp q20, q21, [x21, #464]",
            "stp q22, q23, [x21, #496]",
            "stp q24, q25, [x21, #528]",
            "stp q26, q27, [x21, #560]",
            "stp q28, q29, [x21, #592]",
            "stp q30, q31, [x21, #624]",
                in("x20") from,
                in("x21") to,
                in("x22") stub,
                out("v8") _,
                out("v9") _,
                out("v10") _,
                out("v11") _,
                out("v12") _,
                out("v13") _,
                out("v14") _,
                out("v15") _,
                clobber_abi("C"),
            );
        }
        output
    }

    /// Calls the stub at `stub` with `rax`, `rcx`, `rdx`, `rsi`, `rdi`,
    /// `r8`-`r11` and `xmm0`-`xmm15` loaded from `input`, and returns them
    /// as the stub left them.
    ///
    /// # Safety
    ///
    /// `stub` is one of the slow paths' stubs, and the arguments `input`
    /// passes in registers are ones it accepts. A stub that takes an
    /// argument on the stack reads whatever word the call leaves there, so
    /// that argument must be one the stub ignores.
    #[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
    pub(crate) unsafe fn call_saving(stub: usize, input: &Regs) -> Regs {
        let mut output = Regs {
            x: [0; GENERAL],
            q: [0; VECTOR_WORDS],
        };
        let from = ptr::from_ref(input);
        let to = ptr::from_mut(&mut output);
        // SAFETY: loads every register from `input`, calls the stub, which
        // the caller guarantees, and stores every register into `output`;
        // both live through the block. Everything the block writes is an
        // output or among the C convention's clobbers, and the stack is
        // aligned for a call on entry to the block.
        unsafe {
            core::arch::asm!(
            "movups xmm0, xmmword ptr [r12 + 72]",
            "movups xmm1, xmmword ptr [r12 + 88]",
            "movups xmm2, xmmword ptr [r12 + 104]",
            "movups xmm3, xmmword ptr [r12 + 120]",
            "movups xmm4, xmmword ptr [r12 + 136]",
            "movups xmm5, xmmword ptr [r12 + 152]",
            "movups xmm6, xmmword ptr [r12 + 168]",
            "movups xmm7, xmmword ptr [r12 + 184]",
            "movups xmm8, xmmword ptr [r12 + 200]",
            "movups xmm9, xmmword ptr [r12 + 216]",
            "movups xmm10, xmmword ptr [r12 + 232]",
            "movups xmm11, xmmword ptr [r12 + 248]",
            "movups xmm12, xmmword ptr [r12 + 264]",
            "movups xmm13, xmmword ptr [r12 + 280]",
            "movups xmm14, xmmword ptr [r12 + 296]",
            "movups xmm15, xmmword ptr [r12 + 312]",
            "mov rax, qword ptr [r12]",
            "mov rcx, qword ptr [r12 + 8]",
            "mov rdx, qword ptr [r12 + 16]",
            "mov rsi, qword ptr [r12 + 24]",
            "mov rdi, qword ptr [r12 + 32]",
            "mov r8, qword ptr [r12 + 40]",
            "mov r9, qword ptr [r12 + 48]",
            "mov r10, qword ptr [r12 + 56]",
            "mov r11, qword ptr [r12 + 64]",
            "call r14",
            "mov qword ptr [r13], rax",
            "mov qword ptr [r13 + 8], rcx",
            "mov qword ptr [r13 + 16], rdx",
            "mov qword ptr [r13 + 24], rsi",
            "mov qword ptr [r13 + 32], rdi",
            "mov qword ptr [r13 + 40], r8",
            "mov qword ptr [r13 + 48], r9",
            "mov qword ptr [r13 + 56], r10",
            "mov qword ptr [r13 + 64], r11",
            "movups xmmword ptr [r13 + 72], xmm0",
            "movups xmmword ptr [r13 + 88], xmm1",
            "movups xmmword ptr [r13 + 104], xmm2",
            "movups xmmword ptr [r13 + 120], xmm3",
            "movups xmmword ptr [r13 + 136], xmm4",
            "movups xmmword ptr [r13 + 152], xmm5",
            "movups xmmword ptr [r13 + 168], xmm6",
            "movups xmmword ptr [r13 + 184], xmm7",
            "movups xmmword ptr [r13 + 200], xmm8",
            "movups xmmword ptr [r13 + 216], xmm9",
            "movups xmmword ptr [r13 + 232], xmm10",
            "movups xmmword ptr [r13 + 248], xmm11",
            "movups xmmword ptr [r13 + 264], xmm12",
            "movups xmmword ptr [r13 + 280], xmm13",
            "movups xmmword ptr [r13 + 296], xmm14",
            "movups xmmword ptr [r13 + 312], xmm15",
                in("r12") from,
                in("r13") to,
                in("r14") stub,
                clobber_abi("C"),
            );
        }
        output
    }

    /// The address of the slow path's stub `case` names, the frame size it
    /// is given, and whether it releases words.
    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        not(miri),
        not(windows)
    ))]
    #[expect(clippy::as_conversions, reason = "functions' addresses")]
    fn stub_of(case: usize) -> (usize, Option<u64>, bool) {
        use crate::panic::{cheby_rt_stack_slow, cheby_rt_stack_slow_frame};
        let plain: unsafe extern "C" fn(*mut ExecCtx, Word) = cheby_rt_stack_slow;
        let frame: unsafe extern "C" fn(*mut ExecCtx, Word, u64) = cheby_rt_stack_slow_frame;
        let unwind: unsafe extern "C" fn(*mut ExecCtx, Word, u32, Word, Word, Word, Word) =
            cheby_rt_stack_slow_unwind;
        let frame_unwind: unsafe extern "C" fn(
            *mut ExecCtx,
            Word,
            u64,
            u32,
            Word,
            Word,
            Word,
            Word,
        ) = cheby_rt_stack_slow_frame_unwind;
        match case {
            0 => ((plain as *const ()).addr(), None, false),
            1 => ((frame as *const ()).addr(), Some(1 << 10), false),
            2 => ((unwind as *const ()).addr(), None, true),
            _ => ((frame_unwind as *const ()).addr(), Some(1 << 10), true),
        }
    }

    /// The input of `call_saving` for `stub_of(case)` on `ctx`, every word
    /// a distinct pattern except the stub's arguments: `ctx`, no location,
    /// the frame size `frame` for a large frame's stub and no counted word
    /// for a releasing one.
    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        not(miri),
        not(windows)
    ))]
    fn saving_input(ctx: &ExecCtx, case: usize, frame: u64) -> Regs {
        let mut input = Regs::patterned();
        input.set_argument(
            0,
            u64::try_from(ptr::from_ref(ctx).expose_provenance()).unwrap(),
        );
        input.set_argument(1, 0);
        let (_, size, releases) = stub_of(case);
        let count_at = if size.is_some() {
            input.set_argument(2, frame);
            3
        } else {
            2
        };
        if releases {
            input.set_argument(count_at, 0);
        }
        input
    }

    /// Each slow path's stub gives back every register it saves
    /// ([`Regs`]), the vector registers whole, on each of its ways out:
    /// after a yield it took (the fiber was switched out and back, maybe
    /// onto the other worker),
    /// after a cancellation it delivered and, for a large frame's stubs,
    /// after a stack overflow it raised (the flag is set). Generated code
    /// calls them with `preserve_all`, so a value of the caller may be in
    /// any of them.
    #[test]
    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        not(miri),
        not(windows)
    ))]
    fn entry_slow_stubs_preserve_every_register() {
        for case in 0..4 {
            for way in ["yield", "overflow", "cancel"] {
                let (_, size, _) = stub_of(case);
                if way == "overflow" && size.is_none() {
                    // Only a large frame's stub overflows from here.
                    continue;
                }
                let problems = Problems::default();
                let seen = Arc::clone(&problems);
                let runtime = testing::runtime(2);
                let domain = Domain::new();
                let config = EntryConfig {
                    stack_size: 1 << 20,
                    domain: Arc::clone(&domain),
                    ..EntryConfig::new(EntryMode::Test)
                };
                let outcome = testing::start_root_with(&runtime, &config, move |ctx| {
                    let (stub, size, _) = stub_of(case);
                    let me = ptr::from_ref(ctx).cast_mut();
                    let scope = (way == "cancel").then(|| {
                        // SAFETY: the running fiber's context.
                        let scope = unsafe { cheby_rt_scope_open(me, false, 0, 0) };
                        cancel_running_fiber_at_next_entry(ctx);
                        scope
                    });
                    if way == "yield" {
                        request(ctx, reason::YIELD);
                    }
                    let frame = if way == "overflow" {
                        u64::MAX
                    } else {
                        size.unwrap_or(0)
                    };
                    let input = saving_input(ctx, case, frame);
                    let before = ctx.interrupts();
                    // SAFETY: a stub, with the running fiber's context and
                    // no counted word.
                    let output = unsafe { call_saving(stub, &input) };
                    let context = format!("stub {case}, {way}");
                    if output != input {
                        note(
                            &seen,
                            format!("{context}: {input:x?} came back as {output:x?}"),
                        );
                    }
                    if way == "yield" && ctx.interrupts() == before {
                        note(&seen, format!("{context}: the yield was not handled"));
                    }
                    let flag = crate::unwind::flag::get(ctx);
                    let expected = match way {
                        "yield" => unwind_flag::NONE,
                        "overflow" => unwind_flag::PANIC,
                        _ => unwind_flag::CANCEL,
                    };
                    if flag != expected {
                        note(
                            &seen,
                            format!("{context}: the flag is {flag}, not {expected}"),
                        );
                    }
                    if let Some(scope) = scope {
                        crate::unwind::flag::clear(ctx);
                        // SAFETY: the running fiber's context and its
                        // scope's handle, consumed.
                        if unsafe { cheby_rt_scope_close(me, scope, true) } {
                            note(&seen, format!("{context}: the scope succeeded"));
                        }
                        // SAFETY: the running fiber's context, after a
                        // failed close; the failure is released.
                        unsafe { rc::drop(cheby_fiber_take_failure(me)) };
                    }
                    // An overflow leaves the flag set: the entry panics.
                    7
                })
                .recv_timeout(testing::hang())
                .expect("the root fiber hung");
                runtime.shutdown();
                assert_eq!(domain.live_objects(), 0, "objects leaked");
                if way == "overflow" {
                    let EntryOutcome::Panicked(report) = outcome else {
                        panic!("stub {case}: expected a panic, got {outcome:?}");
                    };
                    assert_eq!(report.kind, PanicKind::StackOverflow);
                } else {
                    assert_eq!(outcome, EntryOutcome::Returned(7), "stub {case}, {way}");
                }
                assert_no_problems(&problems);
            }
        }
    }
}
