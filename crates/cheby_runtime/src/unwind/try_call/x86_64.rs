//! Resuming at a handler on x86-64 with the System V convention (Linux
//! and macOS), for the `try_call` prototype (D-696).

/// Transfers control to the handler at `pc` with the stack pointer `sp`
/// and the frame pointer `fp`, abandoning every frame below.
///
/// Register contract: `rsp` is set from `rcx`, `rbp` from `rsi`, then a
/// `jmp rdi` jumps to the handler. No other register is restored: a
/// `try_call` to a `tail` callee treats every register as clobbered at its
/// handler (Cranelift's `get_regs_clobbered_by_call` with exceptions), so
/// the handler reloads everything it needs from its frame. The exception
/// payload registers `rax` and `rdx` carry nothing.
///
/// # Safety
///
/// `pc` is the handler of a `try_call` in the frame whose frame pointer is
/// `fp`, and `sp` is that frame's stack pointer at the call. Nothing
/// below `sp` is used again: the frames there must hold no value whose
/// destructor still has to run.
pub(crate) unsafe fn resume(pc: usize, sp: usize, fp: usize) -> ! {
    // SAFETY: the caller guarantees `pc`, `sp` and `fp` describe a live
    // handler frame; the jump never returns.
    unsafe {
        std::arch::asm!(
            "mov rsp, rcx",
            "mov rbp, rsi",
            "jmp rdi",
            in("rcx") sp,
            in("rsi") fp,
            in("rdi") pc,
            options(noreturn, nostack),
        )
    }
}
