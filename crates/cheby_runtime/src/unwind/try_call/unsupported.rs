//! Targets without resume code for the `try_call` prototype (D-696):
//! Windows (`WindowsFastcall` cannot be a `try_call` callee, and Windows
//! support is M6, D-545) and every architecture but aarch64 and x86-64.
//! Code compiled for the prototype still runs there until something
//! unwinds, which ends the process.

use crate::panic::fatal;

/// Ends the process: there is no resume code for this target.
///
/// # Safety
///
/// None needed; `unsafe` only to match the supported targets.
pub(crate) unsafe fn resume(pc: usize, sp: usize, fp: usize) -> ! {
    let _ = (pc, sp, fp);
    fatal(format_args!(
        "the try_call unwinding backend cannot resume on this target"
    ))
}
