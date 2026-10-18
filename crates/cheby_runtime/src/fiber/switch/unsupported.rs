//! No switch: Windows until M6, and every architecture without one
//! (D-676, D-714). The runtime checks [`SUPPORTED`] and uses the thread
//! backend instead; calling either function ends the process.

use std::ffi::c_void;

use super::{Saved, StackSlot, checked_top};
use crate::panic::fatal;

/// No switch exists here.
pub const SUPPORTED: bool = false;

/// Ends the process: there is no switch on this platform.
///
/// # Safety
///
/// None needed; it never returns. The signature matches the supported
/// platforms'.
pub unsafe fn prepare(
    slot: &StackSlot,
    stack_top: usize,
    start: unsafe extern "C" fn(*mut c_void) -> !,
    arg: *mut c_void,
) -> Saved {
    let _ = (checked_top(slot, stack_top), start, arg);
    fatal(format_args!(
        "fiber stacks are not supported on this platform (D-714)"
    ))
}

/// Ends the process: there is no switch on this platform.
///
/// # Safety
///
/// None needed; it never returns. The signature matches the supported
/// platforms'.
pub unsafe extern "C" fn switch(from: *mut Saved, to: Saved, msg: usize) -> usize {
    let _ = (from, to, msg);
    fatal(format_args!(
        "fiber switches are not supported on this platform (D-714)"
    ))
}
