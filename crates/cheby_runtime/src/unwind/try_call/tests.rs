//! Tests of the `try_call` prototype's resume code on the machine: a
//! hand-written catching frame (what Cranelift emits around a `try_call`
//! with a handler, reduced to its frame record and saved registers)
//! records its handler, stack and frame pointers; Rust frames below it
//! call the resume code, which must land in the handler with the frame
//! intact and every callee-saved register of the catching frame's caller
//! restored by that frame's epilogue. The walk itself is tested on
//! synthetic frames in `walk.rs`, the whole path through generated code by
//! `cheby_jit`'s `unwind_try_call` tests.

/// Where a catching frame resumes: its handler, and its stack and frame
/// pointers at the call.
#[cfg(not(windows))]
#[repr(C)]
#[derive(Debug, Default)]
struct Catch {
    pc: usize,
    sp: usize,
    fp: usize,
}

/// `catch_frame(catch, body)`: saves the callee-saved registers, stores
/// its handler, stack and frame pointers in `*catch`, calls
/// `body(catch)`, and returns 0 when `body` returns, or 1 when the
/// resume code lands in its handler.
///
/// Register contract (AAPCS64): `x0` is `catch`, `x1` is `body`; `x19` to
/// `x29`, `x30` and `d8` to `d15` are saved in the frame and restored on
/// both paths; `x9` is scratch; the result is in `x0`.
#[cfg(all(target_arch = "aarch64", not(windows)))]
#[unsafe(naked)]
unsafe extern "C" fn catch_frame(catch: *mut Catch, body: unsafe extern "C" fn(*mut Catch)) -> u64 {
    std::arch::naked_asm!(
        "stp x29, x30, [sp, #-96]!",
        "mov x29, sp",
        "stp x19, x20, [sp, #16]",
        "stp x21, x22, [sp, #32]",
        "stp x23, x24, [sp, #48]",
        "stp x25, x26, [sp, #64]",
        "stp x27, x28, [sp, #80]",
        "sub sp, sp, #64",
        "stp d8, d9, [sp]",
        "stp d10, d11, [sp, #16]",
        "stp d12, d13, [sp, #32]",
        "stp d14, d15, [sp, #48]",
        "adr x9, 2f",
        "str x9, [x0]",
        "mov x9, sp",
        "str x9, [x0, #8]",
        "str x29, [x0, #16]",
        "blr x1",
        "mov x0, #0",
        "b 3f",
        "2:",
        "mov x0, #1",
        "3:",
        "ldp d8, d9, [sp]",
        "ldp d10, d11, [sp, #16]",
        "ldp d12, d13, [sp, #32]",
        "ldp d14, d15, [sp, #48]",
        "add sp, sp, #64",
        "ldp x19, x20, [sp, #16]",
        "ldp x21, x22, [sp, #32]",
        "ldp x23, x24, [sp, #48]",
        "ldp x25, x26, [sp, #64]",
        "ldp x27, x28, [sp, #80]",
        "ldp x29, x30, [sp], #96",
        "ret",
    )
}

/// As on aarch64. Register contract (System V): `rdi` is `catch`, `rsi`
/// is `body`; `rbp`, `rbx` and `r12` to `r15` are saved in the frame and
/// restored on both paths; `rax` is scratch and holds the result.
#[cfg(all(target_arch = "x86_64", not(windows)))]
#[unsafe(naked)]
unsafe extern "C" fn catch_frame(catch: *mut Catch, body: unsafe extern "C" fn(*mut Catch)) -> u64 {
    std::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "sub rsp, 8",
        "lea rax, [rip + 2f]",
        "mov [rdi], rax",
        "mov [rdi + 8], rsp",
        "mov [rdi + 16], rbp",
        "call rsi",
        "xor eax, eax",
        "jmp 3f",
        "2:",
        "mov eax, 1",
        "3:",
        "add rsp, 8",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

#[cfg(all(any(target_arch = "aarch64", target_arch = "x86_64"), not(windows)))]
#[expect(clippy::arithmetic_side_effects, reason = "small test counters")]
mod machine {
    use std::hint::black_box;

    use super::{Catch, catch_frame};
    use crate::unwind::try_call::arch;

    /// Recurses `depth` times, scribbling over callee-saved registers on
    /// the way through ordinary Rust code, then resumes in `catch`'s
    /// handler.
    #[inline(never)]
    fn deep(catch: &Catch, depth: u32, acc: u64) -> u64 {
        if depth == 0 {
            // SAFETY: `catch` was filled by `catch_frame`, whose frame is
            // live above this one, and the frames below it hold nothing
            // with a destructor.
            unsafe { arch::resume(catch.pc, catch.sp, catch.fp) }
        }
        let a = black_box(acc.wrapping_mul(31).wrapping_add(u64::from(depth)));
        let b = deep(catch, depth - 1, a);
        black_box(a ^ b)
    }

    unsafe extern "C" fn throws(catch: *mut Catch) {
        // SAFETY: `catch_frame` passes its live record.
        let catch = unsafe { &*catch };
        black_box(deep(catch, 16, 7));
    }

    unsafe extern "C" fn returns(catch: *mut Catch) {
        black_box(catch);
    }

    #[test]
    fn resume_lands_in_the_handler() {
        let mut catch = Catch::default();
        // Values the caller keeps across the call, likely in callee-saved
        // registers.
        let kept: [u64; 8] = black_box([11, 22, 33, 44, 55, 66, 77, 88]);
        let float = black_box(2.5_f64);
        // SAFETY: `catch_frame` follows the C convention and `throws`
        // resumes only into its handler.
        let result = unsafe { catch_frame(&raw mut catch, throws) };
        assert_eq!(result, 1);
        assert_eq!(black_box(kept), [11, 22, 33, 44, 55, 66, 77, 88]);
        assert!((black_box(float) - 2.5).abs() < f64::EPSILON);
        assert_ne!(catch.pc, 0);
        assert_eq!(catch.fp.wrapping_sub(catch.sp) % 16, 0);
    }

    #[test]
    fn a_frame_that_returns_normally_returns_0() {
        let mut catch = Catch::default();
        // SAFETY: as above; `returns` returns.
        let result = unsafe { catch_frame(&raw mut catch, returns) };
        assert_eq!(result, 0);
    }

    #[test]
    fn resume_many_times() {
        for round in 0..1000_u64 {
            let mut catch = Catch::default();
            // SAFETY: as above.
            let result = unsafe { catch_frame(&raw mut catch, throws) };
            assert_eq!(result + round, 1 + round);
        }
    }
}
