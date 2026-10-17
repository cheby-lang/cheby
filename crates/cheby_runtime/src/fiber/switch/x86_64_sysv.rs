//! The switch for the x86-64 System V ABI, on ELF and Mach-O (D-676).
//!
//! A suspended context is 56 bytes at its saved stack pointer, laid out
//! as [`switch`] pushes them:
//!
//! ```text
//! rsp + 48  return address (where to resume)
//! rsp + 40  rbp
//! rsp + 32  rbx
//! rsp + 24  r12
//! rsp + 16  r13
//! rsp +  8  r14
//! rsp +  0  r15
//! ```
//!
//! MXCSR and the x87 control word are not saved: Cheby never changes them.

use std::ffi::c_void;

use super::{Saved, StackSlot, checked_top};
use crate::fiber::sanitizer;

/// The switch exists here.
pub const SUPPORTED: bool = true;

/// A fresh context as [`switch`] restores it, followed by the null
/// return address `start` sees.
#[repr(C)]
struct InitialFrame {
    /// r15, r14, r13.
    r15_r13: [u64; 3],
    /// r12: the argument of `start`.
    r12: *mut c_void,
    /// rbx: `start`.
    rbx: unsafe extern "C" fn(*mut c_void) -> !,
    /// rbp: a null frame pointer.
    rbp: u64,
    /// Where the first switch returns to.
    entry: unsafe extern "C" fn() -> !,
    /// The return address of `start`: null.
    end: u64,
}

const _: () = assert!(size_of::<InitialFrame>() == 64);

/// Lays out a frame just below `stack_top` (aligned down to 16) in
/// `slot`, so that the first [`switch`] to the returned context calls
/// `start(arg)` with a null return address and frame pointer.
///
/// # Safety
///
/// `stack_top` lies in `slot`, above its hard limit, and nothing else uses
/// the memory below it until the context ends; `start` never returns and
/// may assume only what `arg` promises. A `stack_top` outside the slot
/// ends the process.
pub unsafe fn prepare(
    slot: &StackSlot,
    stack_top: usize,
    start: unsafe extern "C" fn(*mut c_void) -> !,
    arg: *mut c_void,
) -> Saved {
    let top = checked_top(slot, stack_top);
    // GAP(sanitize-slot-unpoison): a reused slot may hold the redzones of
    // frames that never returned.
    sanitizer::unpoison_stack(slot.usable_bottom(), top.wrapping_sub(slot.usable_bottom()));
    let frame = top.wrapping_sub(size_of::<InitialFrame>());
    let pointer = std::ptr::with_exposed_provenance_mut::<InitialFrame>(frame);
    // SAFETY: `frame` is 16-aligned and its 64 bytes lie in the slot below
    // `stack_top` (checked above), which the caller gives to this context.
    // The slot's region was mapped with its provenance exposed.
    unsafe {
        pointer.write(InitialFrame {
            r15_r13: [0; 3],
            r12: arg,
            rbx: start,
            rbp: 0,
            entry: fiber_entry,
            end: 0,
        });
    }
    Saved(frame)
}

/// Saves the current context into `*from`, resumes `to`, and returns the
/// message word of whoever switches back to this context.
///
/// Register contract (System V, called from Rust only): on entry rdi is
/// `from`, rsi is `to`'s stack pointer and rdx is `msg`. It pushes rbp,
/// rbx and r12 to r15, writes rsp to `*from`, sets rsp to `to`, pops the
/// same registers from there, and returns to the address above them with
/// rax = `msg` (so the resumed side's call of `switch` returns `msg`). It
/// clobbers, as any call does, every caller-saved register. The `.cfi`
/// rules track every push and pop, and the block has the same layout on
/// both stacks, so a backtrace from inside the switch continues into the
/// context whose stack rsp is on.
///
/// # Safety
///
/// `from` is valid for a write of a `Saved` and stays valid until the
/// context saved there is resumed; `to` is a context saved by `switch` or
/// made by [`prepare`], whose stack is still mapped and not running. No
/// lock guard, borrow or thread-local pointer the caller holds may be used
/// by the code `to` runs (I-5).
#[unsafe(naked)]
pub unsafe extern "C" fn switch(from: *mut Saved, to: Saved, msg: usize) -> usize {
    core::arch::naked_asm!(
        ".cfi_startproc",
        "push rbp",
        ".cfi_adjust_cfa_offset 8",
        ".cfi_rel_offset rbp, 0",
        "push rbx",
        ".cfi_adjust_cfa_offset 8",
        ".cfi_rel_offset rbx, 0",
        "push r12",
        ".cfi_adjust_cfa_offset 8",
        ".cfi_rel_offset r12, 0",
        "push r13",
        ".cfi_adjust_cfa_offset 8",
        ".cfi_rel_offset r13, 0",
        "push r14",
        ".cfi_adjust_cfa_offset 8",
        ".cfi_rel_offset r14, 0",
        "push r15",
        ".cfi_adjust_cfa_offset 8",
        ".cfi_rel_offset r15, 0",
        // Publish this context and move to the other one.
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        ".cfi_adjust_cfa_offset -8",
        ".cfi_restore r15",
        "pop r14",
        ".cfi_adjust_cfa_offset -8",
        ".cfi_restore r14",
        "pop r13",
        ".cfi_adjust_cfa_offset -8",
        ".cfi_restore r13",
        "pop r12",
        ".cfi_adjust_cfa_offset -8",
        ".cfi_restore r12",
        "pop rbx",
        ".cfi_adjust_cfa_offset -8",
        ".cfi_restore rbx",
        "pop rbp",
        ".cfi_adjust_cfa_offset -8",
        ".cfi_restore rbp",
        "mov rax, rdx",
        "ret",
        ".cfi_endproc",
    )
}

/// Where the first switch to a fresh context returns to.
///
/// Register contract: entered by `ret` from [`switch`] with rsp pointing
/// at the null word below the prepared stack top (so rsp + 8 is
/// 16-aligned, as after a call), r12 = `arg`, rbx = `start`, rbp = 0; rax
/// holds the dropped first message. It sets rdi = `arg` and jumps (not
/// calls) to `start`, which thus sees a null return address and a null
/// frame pointer, ending frame-pointer walks; the `.cfi_undefined` rule
/// ends DWARF unwinding here. `start` never returns; if it did, it would
/// return to address 0 and fault.
///
/// # Safety
///
/// Only [`switch`] enters it, through a frame made by [`prepare`].
#[unsafe(naked)]
unsafe extern "C" fn fiber_entry() -> ! {
    core::arch::naked_asm!(
        ".cfi_startproc",
        ".cfi_undefined rip",
        "mov rdi, r12",
        "xor ebp, ebp",
        "jmp rbx",
        "ud2",
        ".cfi_endproc",
    )
}
