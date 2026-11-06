//! Tests of the context switch (D-676): callee-saved registers survive
//! round trips, a million switches, the first frame of a fresh stack, any
//! pair of contexts, migration between threads, and backtraces.
//!
//! `CHEBY_RT_TEST_EMULATED=1` (set by the linux/amd64 container runs; macOS
//! x86-64 is dropped, D-892) keeps every check but the time bound.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "test arithmetic on counters and register patterns"
)]

use std::ffi::c_void;
use std::time::{Duration, Instant};

use super::*;
use crate::fiber::{StackPool, StackSlot};

/// Whether the tests run under an emulator, where only correctness is
/// asserted.
fn emulated() -> bool {
    std::env::var_os("CHEBY_RT_TEST_EMULATED").is_some_and(|value| value == "1")
}

/// A slot of the global pool for one test.
fn slot(bytes: usize) -> StackSlot {
    StackPool::global().take(bytes).unwrap()
}

/// Gives a slot back, releasing everything below its warm top.
fn give_back(slot: StackSlot) {
    let bottom = slot.bottom();
    StackPool::global().give_back(slot, bottom);
}

/// Two contexts that switch to each other, shared through a raw pointer
/// by both sides.
#[derive(Default)]
struct Pair {
    /// The test thread's context while the fiber runs.
    main: Saved,
    /// The fiber's context while the test thread runs.
    fiber: Saved,
    /// Register mismatches the fiber saw.
    failures: usize,
    /// Round trips the fiber made.
    rounds: usize,
}

/// A fiber that answers every message `m` with `m + 1`, after a first
/// answer 0.
unsafe extern "C" fn echo(arg: *mut c_void) -> ! {
    let pair = arg.cast::<Pair>();
    let mut reply = 0;
    loop {
        // SAFETY: `pair` outlives the fiber's use of it, and only one side
        // runs at a time.
        let message = unsafe { switch(&raw mut (*pair).fiber, (*pair).main.take(), reply) };
        reply = message + 1;
    }
}

/// Starts a fiber on `slot` running `start` with `arg`, switching to it
/// once; returns its first message.
///
/// # Safety
///
/// `pair` is live and `start` uses it as `echo` does.
unsafe fn start_on(
    slot: &StackSlot,
    pair: *mut Pair,
    start: unsafe extern "C" fn(*mut c_void) -> !,
) -> usize {
    // SAFETY: forwarded from the caller; the slot is this test's.
    unsafe {
        (*pair).fiber = prepare(slot, slot.stack_top(), start, pair.cast());
        switch(&raw mut (*pair).main, (*pair).fiber.take(), 12345)
    }
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn million_switches() {
    let stack = slot(1 << 20);
    let pair = Box::into_raw(Box::default());
    // SAFETY: `pair` lives until the end; the fiber only answers.
    let first = unsafe { start_on(&stack, pair, echo) };
    assert_eq!(first, 0, "the first message to a fresh fiber is dropped");
    let started = Instant::now();
    for round in 0..1_000_000_usize {
        // SAFETY: the fiber is suspended in `switch`.
        let answer = unsafe { switch(&raw mut (*pair).main, (*pair).fiber.take(), round) };
        assert_eq!(answer, round + 1);
    }
    let elapsed = started.elapsed();
    println!("1,000,000 round trips in {elapsed:?}");
    if !emulated() {
        let bound = crate::sched::testing::scaled(Duration::from_secs(10));
        assert!(elapsed < bound, "{elapsed:?}");
    }
    // SAFETY: the fiber is suspended forever and holds nothing to drop.
    drop(unsafe { Box::from_raw(pair) });
    give_back(stack);
}

/// Test-only helpers that fill every callee-saved register with a
/// pattern, switch, and report what the registers hold afterwards.
#[cfg(all(target_arch = "aarch64", not(windows)))]
mod regs {
    use super::Saved;

    /// x19 to x28, x29, then d8 to d15.
    pub(super) const COUNT: usize = 19;

    /// Loads `seed + 1` to `seed + 19` into x19 to x28, x29 and d8 to
    /// d15, calls `switch(from, to, msg)`, stores those registers into
    /// `*out` and returns the switch's result, restoring the caller's
    /// registers.
    ///
    /// # Safety
    ///
    /// As for `switch`; `out` is valid for a write of `COUNT` words.
    #[unsafe(naked)]
    pub(super) unsafe extern "C" fn fill_switch_report(
        from: *mut Saved,
        to: Saved,
        msg: usize,
        seed: u64,
        out: *mut [u64; COUNT],
    ) -> usize {
        core::arch::naked_asm!(
            "sub sp, sp, #176",
            "stp d8, d9, [sp, #0]",
            "stp d10, d11, [sp, #16]",
            "stp d12, d13, [sp, #32]",
            "stp d14, d15, [sp, #48]",
            "stp x19, x20, [sp, #64]",
            "stp x21, x22, [sp, #80]",
            "stp x23, x24, [sp, #96]",
            "stp x25, x26, [sp, #112]",
            "stp x27, x28, [sp, #128]",
            "stp x29, x30, [sp, #144]",
            "str x4, [sp, #160]",
            "add x19, x3, #1",
            "add x20, x3, #2",
            "add x21, x3, #3",
            "add x22, x3, #4",
            "add x23, x3, #5",
            "add x24, x3, #6",
            "add x25, x3, #7",
            "add x26, x3, #8",
            "add x27, x3, #9",
            "add x28, x3, #10",
            "add x29, x3, #11",
            "add x9, x3, #12",
            "fmov d8, x9",
            "add x9, x3, #13",
            "fmov d9, x9",
            "add x9, x3, #14",
            "fmov d10, x9",
            "add x9, x3, #15",
            "fmov d11, x9",
            "add x9, x3, #16",
            "fmov d12, x9",
            "add x9, x3, #17",
            "fmov d13, x9",
            "add x9, x3, #18",
            "fmov d14, x9",
            "add x9, x3, #19",
            "fmov d15, x9",
            "bl {switch}",
            "ldr x9, [sp, #160]",
            "stp x19, x20, [x9, #0]",
            "stp x21, x22, [x9, #16]",
            "stp x23, x24, [x9, #32]",
            "stp x25, x26, [x9, #48]",
            "stp x27, x28, [x9, #64]",
            "str x29, [x9, #80]",
            "stp d8, d9, [x9, #88]",
            "stp d10, d11, [x9, #104]",
            "stp d12, d13, [x9, #120]",
            "stp d14, d15, [x9, #136]",
            "ldp d8, d9, [sp, #0]",
            "ldp d10, d11, [sp, #16]",
            "ldp d12, d13, [sp, #32]",
            "ldp d14, d15, [sp, #48]",
            "ldp x19, x20, [sp, #64]",
            "ldp x21, x22, [sp, #80]",
            "ldp x23, x24, [sp, #96]",
            "ldp x25, x26, [sp, #112]",
            "ldp x27, x28, [sp, #128]",
            "ldp x29, x30, [sp, #144]",
            "add sp, sp, #176",
            "ret",
            switch = sym super::switch,
        )
    }

    /// Records the frame pointer, return address and stack pointer that
    /// `start` sees into the first three words of `arg`, then continues
    /// in `probe_continue`.
    ///
    /// # Safety
    ///
    /// Used only as a fiber's `start`, with `arg` a `Probe`.
    #[unsafe(naked)]
    pub(super) unsafe extern "C" fn probe_entry(arg: *mut core::ffi::c_void) -> ! {
        core::arch::naked_asm!(
            "str x30, [x0, #0]",
            "str x29, [x0, #8]",
            "mov x9, sp",
            "str x9, [x0, #16]",
            "b {next}",
            next = sym super::probe_continue,
        )
    }

    /// The stack pointer `start` sees, from the prepared stack top.
    pub(super) fn entry_sp(stack_top: usize) -> usize {
        stack_top
    }
}

/// Test-only helpers that fill every callee-saved register with a
/// pattern, switch, and report what the registers hold afterwards.
#[cfg(all(target_arch = "x86_64", not(windows)))]
mod regs {
    use super::Saved;

    /// rbx, rbp, r12, r13, r14, r15.
    pub(super) const COUNT: usize = 6;

    /// Loads `seed + 1` to `seed + 6` into rbx, rbp and r12 to r15, calls
    /// `switch(from, to, msg)`, stores those registers into `*out` and
    /// returns the switch's result, restoring the caller's registers.
    ///
    /// # Safety
    ///
    /// As for `switch`; `out` is valid for a write of `COUNT` words.
    #[unsafe(naked)]
    pub(super) unsafe extern "C" fn fill_switch_report(
        from: *mut Saved,
        to: Saved,
        msg: usize,
        seed: u64,
        out: *mut [u64; COUNT],
    ) -> usize {
        core::arch::naked_asm!(
            "push rbp",
            "push rbx",
            "push r12",
            "push r13",
            "push r14",
            "push r15",
            // Seven pushes leave rsp 16-aligned for the call.
            "push r8",
            "lea rbx, [rcx + 1]",
            "lea rbp, [rcx + 2]",
            "lea r12, [rcx + 3]",
            "lea r13, [rcx + 4]",
            "lea r14, [rcx + 5]",
            "lea r15, [rcx + 6]",
            "call {switch}",
            "mov r8, [rsp]",
            "mov [r8], rbx",
            "mov [r8 + 8], rbp",
            "mov [r8 + 16], r12",
            "mov [r8 + 24], r13",
            "mov [r8 + 32], r14",
            "mov [r8 + 40], r15",
            "pop r8",
            "pop r15",
            "pop r14",
            "pop r13",
            "pop r12",
            "pop rbx",
            "pop rbp",
            "ret",
            switch = sym super::switch,
        )
    }

    /// Records the frame pointer, return address and stack pointer that
    /// `start` sees into the first three words of `arg`, then continues
    /// in `probe_continue`.
    ///
    /// # Safety
    ///
    /// Used only as a fiber's `start`, with `arg` a `Probe`.
    #[unsafe(naked)]
    pub(super) unsafe extern "C" fn probe_entry(arg: *mut core::ffi::c_void) -> ! {
        core::arch::naked_asm!(
            "mov rax, [rsp]",
            "mov [rdi], rax",
            "mov [rdi + 8], rbp",
            "mov [rdi + 16], rsp",
            "jmp {next}",
            next = sym super::probe_continue,
        )
    }

    /// The stack pointer `start` sees, from the prepared stack top: one
    /// word below it, as after a call.
    pub(super) fn entry_sp(stack_top: usize) -> usize {
        stack_top - 8
    }
}

/// Stand-ins where there is no switch; the tests that use them are
/// ignored there.
#[cfg(any(windows, not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
mod regs {
    use super::Saved;

    /// No registers.
    pub(super) const COUNT: usize = 1;

    /// Never called: the tests are ignored here.
    pub(super) unsafe extern "C" fn fill_switch_report(
        _from: *mut Saved,
        _to: Saved,
        _msg: usize,
        _seed: u64,
        _out: *mut [u64; COUNT],
    ) -> usize {
        panic!("no switch on this platform")
    }

    /// Never called: the tests are ignored here.
    pub(super) unsafe extern "C" fn probe_entry(arg: *mut core::ffi::c_void) -> ! {
        // SAFETY: never reached; the signature is the supported platforms'.
        unsafe { super::probe_continue(arg) }
    }

    /// Never called: the tests are ignored here.
    pub(super) fn entry_sp(stack_top: usize) -> usize {
        stack_top
    }
}

/// The values `fill_switch_report` loads for `seed`.
fn pattern(seed: u64) -> [u64; regs::COUNT] {
    std::array::from_fn(|index| seed + 1 + u64::try_from(index).unwrap())
}

/// The fiber's seed in round `round`.
fn fiber_seed(round: usize) -> u64 {
    0x5a5a_0000_0000_0000 | (u64::try_from(round).unwrap() << 8)
}

/// The test thread's seed in round `round`.
fn main_seed(round: usize) -> u64 {
    0xa5a5_0000_0000_0000 | (u64::try_from(round).unwrap() << 8)
}

/// A fiber that fills its callee-saved registers with its own pattern at
/// every switch and counts the rounds in which they came back changed.
unsafe extern "C" fn filler(arg: *mut c_void) -> ! {
    let pair = arg.cast::<Pair>();
    let mut out = [0; regs::COUNT];
    let mut round = 0;
    loop {
        // Clobber registers with ordinary Rust work between switches.
        let noise: u64 = (0..u64::try_from(round % 7).unwrap()).map(|x| x * x).sum();
        std::hint::black_box(noise);
        // SAFETY: `pair` outlives the fiber's use of it, only one side runs
        // at a time, and `out` is a local array of `COUNT` words.
        let message = unsafe {
            regs::fill_switch_report(
                &raw mut (*pair).fiber,
                (*pair).main.take(),
                round,
                fiber_seed(round),
                &raw mut out,
            )
        };
        // SAFETY: as above.
        unsafe {
            if out != pattern(fiber_seed(round)) {
                (*pair).failures += 1;
            }
            (*pair).rounds += 1;
        }
        round = message;
    }
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn switch_preserves_callee_saved() {
    let stack = slot(1 << 20);
    let pair = Box::into_raw(Box::default());
    // SAFETY: `pair` lives until the end; `filler` uses it as `echo` does.
    let first = unsafe { start_on(&stack, pair, filler) };
    assert_eq!(first, 0);
    let mut out = [0; regs::COUNT];
    for round in 1..=10_000 {
        // SAFETY: the fiber is suspended in `switch`; `out` is local.
        let reply = unsafe {
            regs::fill_switch_report(
                &raw mut (*pair).main,
                (*pair).fiber.take(),
                round,
                main_seed(round),
                &raw mut out,
            )
        };
        assert_eq!(reply, round, "the fiber answers with its round");
        assert_eq!(
            out,
            pattern(main_seed(round)),
            "registers after round {round}"
        );
    }
    // SAFETY: the fiber is suspended.
    let pair = unsafe { Box::from_raw(pair) };
    assert_eq!(pair.rounds, 10_000);
    assert_eq!(pair.failures, 0, "the fiber's registers changed");
    give_back(stack);
}

/// What `probe_entry` saw, then the two contexts.
#[repr(C)]
#[derive(Default)]
struct Probe {
    /// The return address `start` saw.
    return_address: usize,
    /// The frame pointer `start` saw.
    frame_pointer: usize,
    /// The stack pointer `start` saw.
    stack_pointer: usize,
    /// The test thread's context.
    main: Saved,
    /// The fiber's context.
    fiber: Saved,
}

/// Where `probe_entry` continues: switches back and never runs again.
unsafe extern "C" fn probe_continue(arg: *mut c_void) -> ! {
    let probe = arg.cast::<Probe>();
    loop {
        // SAFETY: `probe` outlives the fiber's use of it.
        unsafe { switch(&raw mut (*probe).fiber, (*probe).main.take(), 7) };
    }
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn the_first_frame_has_a_null_return_address_and_frame_pointer() {
    let mut stack = slot(1 << 20);
    stack.reserve_top(100, 64).unwrap();
    let top = stack.stack_top();
    let probe = Box::into_raw(Box::<Probe>::default());
    // SAFETY: `probe` lives until the end; the slot is this test's.
    let message = unsafe {
        (*probe).fiber = prepare(&stack, top, regs::probe_entry, probe.cast());
        switch(&raw mut (*probe).main, (*probe).fiber.take(), 1)
    };
    assert_eq!(message, 7);
    // SAFETY: the fiber is suspended.
    let probe = unsafe { Box::from_raw(probe) };
    assert_eq!(probe.return_address, 0);
    assert_eq!(probe.frame_pointer, 0);
    assert_eq!(probe.stack_pointer, regs::entry_sp(top));
    give_back(stack);
}

/// A closure to run on a fiber, and its result.
struct Run<F, T> {
    /// The test thread's context.
    main: Saved,
    /// The fiber's context.
    fiber: Saved,
    /// The closure, taken by the fiber.
    body: Option<F>,
    /// Its result.
    result: Option<T>,
}

/// Runs the closure of a `Run`, stores its result and switches back for
/// good.
unsafe extern "C" fn run_body<F: FnOnce() -> T, T>(arg: *mut c_void) -> ! {
    let run = arg.cast::<Run<F, T>>();
    // SAFETY: `run` outlives the fiber's use of it.
    unsafe {
        let body = (*run).body.take().unwrap();
        (*run).result = Some(body());
    }
    loop {
        // SAFETY: as above.
        unsafe { switch(&raw mut (*run).fiber, (*run).main.take(), 0) };
    }
}

/// Runs `body` on a fresh fiber stack of `bytes` and returns its result.
/// A panic in `body` aborts the test binary (the fiber's base frame is
/// `extern "C"`).
fn on_fiber<T, F: FnOnce() -> T>(bytes: usize, body: F) -> T {
    let stack = slot(bytes);
    let run = Box::into_raw(Box::new(Run {
        main: Saved::EMPTY,
        fiber: Saved::EMPTY,
        body: Some(body),
        result: None,
    }));
    // SAFETY: `run` lives until the fiber has switched back for good.
    let run = unsafe {
        (*run).fiber = prepare(&stack, stack.stack_top(), run_body::<F, T>, run.cast());
        switch(&raw mut (*run).main, (*run).fiber.take(), 0);
        Box::from_raw(run)
    };
    give_back(stack);
    run.result.unwrap()
}

/// Recursion that uses about `depth` KiB of stack.
#[inline(never)]
fn deep(depth: usize) -> usize {
    let frame = std::hint::black_box([u8::try_from(depth % 256).unwrap(); 1024]);
    if depth == 0 {
        return usize::from(frame[0]);
    }
    deep(depth - 1) + usize::from(frame[1023] & 1)
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_fiber_runs_deep_rust_code() {
    // About 4 MiB of an 8 MiB slot, far above its hard limit.
    let sum = on_fiber(8 << 20, || deep(4096));
    assert_eq!(
        sum,
        deep(0) + (0..=4096).filter(|depth| depth & 1 == 1).count()
    );
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_backtrace_in_a_fiber_ends_at_its_first_frame() {
    let text = on_fiber(1 << 20, || {
        std::backtrace::Backtrace::force_capture().to_string()
    });
    assert!(text.contains("run_body"), "{text}");
    // The walk stops at the fiber's first frame instead of running into
    // the test thread's frames or garbage.
    assert!(!text.contains("lang_start"), "{text}");
    assert!(!text.contains("on_fiber"), "{text}");
    assert!(text.lines().count() < 200, "{text}");
}

/// Three contexts: the test thread and two fibers that switch to each
/// other directly.
#[derive(Default)]
struct Trio {
    /// The test thread's context.
    main: Saved,
    /// Fiber A's context.
    a: Saved,
    /// Fiber B's context.
    b: Saved,
    /// The messages B received from A, in order.
    seen: Vec<usize>,
}

/// Rounds of A and B before B returns to the test thread.
const TRIO_ROUNDS: usize = 1000;

/// Fiber A: sends 0, 2, 4, ... to B, expecting odd answers back.
unsafe extern "C" fn fiber_a(arg: *mut c_void) -> ! {
    let trio = arg.cast::<Trio>();
    let mut next = 0;
    loop {
        // SAFETY: `trio` outlives both fibers' use of it.
        let answer = unsafe { switch(&raw mut (*trio).a, (*trio).b.take(), next) };
        next = answer + 1;
    }
}

/// Fiber B: answers each message with message + 1, and after
/// `TRIO_ROUNDS` switches to the test thread instead.
unsafe extern "C" fn fiber_b(arg: *mut c_void) -> ! {
    let trio = arg.cast::<Trio>();
    // B starts from the test thread, which sent it nothing useful: it
    // starts A with its own first switch.
    // SAFETY: `trio` outlives both fibers' use of it.
    let mut message = unsafe { switch(&raw mut (*trio).b, (*trio).a.take(), 0) };
    loop {
        // SAFETY: as above.
        unsafe {
            (*trio).seen.push(message);
            let to = if (*trio).seen.len() == TRIO_ROUNDS {
                (*trio).main.take()
            } else {
                (*trio).a.take()
            };
            message = switch(&raw mut (*trio).b, to, message + 1);
        }
    }
}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn two_fibers_switch_to_each_other() {
    let (stack_a, stack_b) = (slot(1 << 20), slot(1 << 20));
    let trio = Box::into_raw(Box::<Trio>::default());
    // SAFETY: `trio` lives until both fibers are suspended for good.
    let trio = unsafe {
        (*trio).a = prepare(&stack_a, stack_a.stack_top(), fiber_a, trio.cast());
        (*trio).b = prepare(&stack_b, stack_b.stack_top(), fiber_b, trio.cast());
        let last = switch(&raw mut (*trio).main, (*trio).b.take(), 99);
        assert_eq!(last, 2 * TRIO_ROUNDS - 1);
        Box::from_raw(trio)
    };
    // A's first message (to B's start) is dropped; B's first switch
    // starts A, whose first message is 0.
    let expected: Vec<usize> = (0..TRIO_ROUNDS).map(|round| 2 * round).collect();
    assert_eq!(trio.seen, expected);
    give_back(stack_a);
    give_back(stack_b);
}

/// A context that is safe to move between threads in a test.
struct SendPair(*mut Pair);

// SAFETY: the test hands the pointer from one thread to the next and
// joins each thread before the next one uses it.
unsafe impl Send for SendPair {}

#[test]
#[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
fn a_fiber_resumes_on_another_thread() {
    let stack = slot(1 << 20);
    let pair = SendPair(Box::into_raw(Box::default()));
    // SAFETY: `pair` lives until the end; the fiber only answers.
    let first = unsafe { start_on(&stack, pair.0, echo) };
    assert_eq!(first, 0);
    let mut pair = pair;
    for thread in 0..8_usize {
        pair = std::thread::spawn(move || {
            let shared = pair;
            for round in 0..100 {
                let message = thread * 1000 + round;
                // SAFETY: the fiber is suspended; this thread is the only
                // one using the pair now.
                let answer =
                    unsafe { switch(&raw mut (*shared.0).main, (*shared.0).fiber.take(), message) };
                assert_eq!(answer, message + 1);
            }
            shared
        })
        .join()
        .unwrap();
    }
    // SAFETY: the fiber is suspended.
    drop(unsafe { Box::from_raw(pair.0) });
    give_back(stack);
}

#[test]
fn saved_contexts_move_out() {
    let mut saved = Saved(0x1000);
    assert!(!saved.is_empty());
    let moved = saved.take();
    assert!(saved.is_empty());
    assert!(!moved.is_empty());
    assert!(Saved::EMPTY.is_empty());
    assert_eq!(
        SUPPORTED,
        cfg!(all(
            not(windows),
            any(target_arch = "aarch64", target_arch = "x86_64")
        ))
    );
}
