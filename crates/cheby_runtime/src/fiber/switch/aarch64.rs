//! The switch for AAPCS64, on Linux and Apple (D-676).
//!
//! A suspended context is 160 bytes at its saved stack pointer, laid out
//! as [`switch`] stores them:
//!
//! ```text
//! sp + 144  x29 (frame pointer), x30 (link register: where to resume)
//! sp + 128  x27, x28
//! sp + 112  x25, x26
//! sp +  96  x23, x24
//! sp +  80  x21, x22
//! sp +  64  x19, x20
//! sp +  48  d14, d15
//! sp +  32  d12, d13
//! sp +  16  d10, d11
//! sp +   0  d8,  d9
//! ```
//!
//! x18 is the platform register (reserved on Apple) and is never touched.
//! The switch returns with `ret`, so branch target identification never
//! applies to it.

use std::ffi::c_void;

use super::{Saved, StackSlot, checked_top};

/// The switch exists here.
pub const SUPPORTED: bool = true;

/// A fresh context as [`switch`] restores it: zero in every register
/// but the ones [`fiber_entry`] reads.
#[repr(C)]
struct InitialFrame {
    /// d8 to d15.
    d8_d15: [u64; 8],
    /// x19: the argument of `start`.
    x19: *mut c_void,
    /// x20: `start`.
    x20: unsafe extern "C" fn(*mut c_void) -> !,
    /// x21 to x28.
    x21_x28: [u64; 8],
    /// x29: a null frame pointer.
    x29: u64,
    /// x30: where the first switch returns to.
    x30: unsafe extern "C" fn() -> !,
}

const _: () = assert!(size_of::<InitialFrame>() == 160);

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
    let frame = top.wrapping_sub(size_of::<InitialFrame>());
    let pointer = std::ptr::with_exposed_provenance_mut::<InitialFrame>(frame);
    // SAFETY: `frame` is 16-aligned and its 160 bytes lie in the slot
    // below `stack_top` (checked above), which the caller gives to this
    // context. The slot's region was mapped with its provenance exposed.
    unsafe {
        pointer.write(InitialFrame {
            d8_d15: [0; 8],
            x19: arg,
            x20: start,
            x21_x28: [0; 8],
            x29: 0,
            x30: fiber_entry,
        });
    }
    Saved(frame)
}

/// Saves the current context into `*from`, resumes `to`, and returns the
/// message word of whoever switches back to this context.
///
/// Register contract (AAPCS64, called from Rust only): on entry x0 is
/// `from`, x1 is `to`'s stack pointer and x2 is `msg`. It stores x19 to
/// x28, x29, x30 and d8 to d15 in 160 bytes below sp, writes sp to
/// `*from`, sets sp to `to`, reloads the same registers from there, and
/// returns to the reloaded x30 with x0 = `msg` (so the resumed side's
/// call of `switch` returns `msg`). It clobbers x9 and, as any call
/// does, every caller-saved register; x18 is never touched. The `.cfi`
/// rules describe the 160-byte block at every instruction, which is the
/// same layout on both stacks, so a backtrace from inside the switch
/// continues into the context whose stack sp is on.
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
        "sub sp, sp, #160",
        ".cfi_def_cfa_offset 160",
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
        ".cfi_offset x30, -8",
        ".cfi_offset x29, -16",
        ".cfi_offset x28, -24",
        ".cfi_offset x27, -32",
        ".cfi_offset x26, -40",
        ".cfi_offset x25, -48",
        ".cfi_offset x24, -56",
        ".cfi_offset x23, -64",
        ".cfi_offset x22, -72",
        ".cfi_offset x21, -80",
        ".cfi_offset x20, -88",
        ".cfi_offset x19, -96",
        ".cfi_offset d15, -104",
        ".cfi_offset d14, -112",
        ".cfi_offset d13, -120",
        ".cfi_offset d12, -128",
        ".cfi_offset d11, -136",
        ".cfi_offset d10, -144",
        ".cfi_offset d9, -152",
        ".cfi_offset d8, -160",
        // Publish this context and move to the other one.
        "mov x9, sp",
        "str x9, [x0]",
        "mov sp, x1",
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
        "add sp, sp, #160",
        ".cfi_def_cfa_offset 0",
        ".cfi_restore x30",
        ".cfi_restore x29",
        ".cfi_restore x28",
        ".cfi_restore x27",
        ".cfi_restore x26",
        ".cfi_restore x25",
        ".cfi_restore x24",
        ".cfi_restore x23",
        ".cfi_restore x22",
        ".cfi_restore x21",
        ".cfi_restore x20",
        ".cfi_restore x19",
        ".cfi_restore d15",
        ".cfi_restore d14",
        ".cfi_restore d13",
        ".cfi_restore d12",
        ".cfi_restore d11",
        ".cfi_restore d10",
        ".cfi_restore d9",
        ".cfi_restore d8",
        "mov x0, x2",
        "ret",
        ".cfi_endproc",
    )
}

/// Where the first switch to a fresh context returns to.
///
/// Register contract: entered by `ret` from [`switch`] with sp at the
/// prepared stack top (16-aligned), x19 = `arg`, x20 = `start`, x29 = 0;
/// x0 holds the dropped first message. It sets x0 = `arg`, x30 = 0 and
/// branches (not calls) to `start` through x16, which every branch target
/// identification landing pad accepts. `start` thus sees a null frame
/// pointer and a null return address, and saves them as its frame record,
/// which ends frame-pointer walks; the `.cfi_undefined` rule ends DWARF
/// unwinding here. `start` never returns; if it did, x30 = 0 would fault.
///
/// # Safety
///
/// Only [`switch`] enters it, through a frame made by [`prepare`].
#[unsafe(naked)]
unsafe extern "C" fn fiber_entry() -> ! {
    core::arch::naked_asm!(
        ".cfi_startproc",
        ".cfi_undefined x30",
        "mov x0, x19",
        "mov x16, x20",
        "mov x29, xzr",
        "mov x30, xzr",
        "br x16",
        "brk #0xc4eb",
        ".cfi_endproc",
    )
}
