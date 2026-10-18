//! The context switch between stacks (D-676): our own `naked_asm!`
//! switch for x86-64 System V (ELF and Mach-O) and AAPCS64 (Linux and
//! Apple).
//! Windows and every other target get [`unsupported`]'s stubs, with
//! [`SUPPORTED`] false, until M6 (D-714).
//!
//! The switch is called only from Rust, so it saves exactly the C
//! callee-saved registers of the platform: the generated code's frames
//! above it have already spilled what a C call clobbers. Floating-point
//! control state (MXCSR, the x87 control word, FPCR) is not saved: Cheby
//! never changes it.
//!
//! - [`prepare`] lays out a fresh stack so that the first switch to it
//!   calls `start(arg)` with a null return address and frame pointer,
//!   which ends every frame-pointer walk and debugger backtrace.
//! - [`switch`] saves the current context into `*from`, resumes `to`, and
//!   returns the other side's message word when something switches back.
//!   The scheduler uses it asymmetrically (a fiber always switches to its
//!   worker's stack, with a post-switch action in the message word), but
//!   any pair of contexts works.
//!
//! The first message sent to a fresh context is dropped: `start` receives
//! only `arg`.

#[cfg(all(target_arch = "aarch64", not(windows)))]
mod aarch64;
#[cfg(all(target_arch = "aarch64", not(windows)))]
pub use aarch64::{SUPPORTED, prepare, switch};

#[cfg(all(target_arch = "x86_64", not(windows)))]
mod x86_64_sysv;
#[cfg(all(target_arch = "x86_64", not(windows)))]
pub use x86_64_sysv::{SUPPORTED, prepare, switch};

#[cfg(any(windows, not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
mod unsupported;
#[cfg(any(windows, not(any(target_arch = "aarch64", target_arch = "x86_64"))))]
pub use unsupported::{SUPPORTED, prepare, switch};

use super::StackSlot;
use crate::panic::fatal;

/// A suspended context: the stack pointer at which [`switch`] saved its
/// callee-saved registers, or empty. It is not `Copy`: resuming a context
/// consumes it, and a context can be resumed only once per suspension.
#[repr(transparent)]
#[derive(Debug, Default)]
pub struct Saved(usize);

impl Saved {
    /// No context: what a [`switch`]'s `from` holds before the switch
    /// writes it.
    pub const EMPTY: Saved = Saved(0);

    /// Whether this holds no context.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Moves the context out, leaving [`Saved::EMPTY`].
    #[must_use]
    pub fn take(&mut self) -> Saved {
        std::mem::take(self)
    }
}

/// The lowest `stack_top` [`prepare`] accepts: room for its frame and a
/// few frames of `start`, above the slot's hard limit.
const MIN_START_ROOM: usize = 4096;

/// Checks the arguments of [`prepare`] and returns `stack_top` aligned
/// down to 16 bytes; a stack top outside the slot ends the process.
fn checked_top(slot: &StackSlot, stack_top: usize) -> usize {
    let top = stack_top & !15;
    let lowest = slot.hard_limit().saturating_add(MIN_START_ROOM);
    if stack_top > slot.top() || top < lowest {
        fatal(format_args!(
            "fiber stack top {stack_top:#x} outside its slot {:#x}..{:#x}",
            slot.bottom(),
            slot.top()
        ));
    }
    top
}

#[cfg(test)]
mod tests;
