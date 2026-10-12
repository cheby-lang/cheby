//! The `try_call` prototype (D-696, D-702): the runtime side of the
//! unwinding backend that resumes at Cranelift `try_call` handlers. It is
//! one of two prototypes measured before the user picks one (D-696);
//! the hidden `CHEBY_UNWIND=try_call` selects it.
//!
//! Generated code calls every C function that may unwind through an exit
//! stub, which records its caller's frame pointer and the return address
//! of the caller's `try_call` at `ctx+16` and `ctx+24`
//! (GAP(try-call-exit-record)). When the unwinding reaches Rust's side
//! (a guard caught the marker, `cheby_rt_panic` or a stack slow path
//! recorded a panic) or a cleanup pad ends in `Resume`, the runtime:
//!
//! 1. reads the exit record,
//! 2. walks the frame-pointer chain from it (`walk`) to the first frame
//!    whose return address has a handler in the process registry
//!    (`registry`, filled by the JIT with each function's table), and
//! 3. jumps to the handler with that frame's stack and frame pointers
//!    (`aarch64.rs`, `x86_64.rs`; other targets end the process,
//!    `unsupported.rs`).
//!
//! The handler runs the MIR pad, which drops what its frame owns and
//! either ends in `Resume` (back here, one frame up) or, at a catch point
//! (an entry thunk or a trampoline), returns 0. Every frame between the
//! throw and the handler is abandoned without running destructors, so the
//! callers drop what they hold before they throw (plan §2.5). The record
//! of the unwinding and the flag are the shared runtime's; these functions
//! only transfer control.

mod registry;
mod walk;

#[cfg(all(target_arch = "aarch64", not(windows)))]
#[path = "aarch64.rs"]
mod arch;
#[cfg(all(target_arch = "x86_64", not(windows)))]
#[path = "x86_64.rs"]
mod arch;
#[cfg(not(any(
    all(target_arch = "aarch64", not(windows)),
    all(target_arch = "x86_64", not(windows))
)))]
#[path = "unsupported.rs"]
mod arch;

#[cfg(test)]
mod tests;

use std::sync::atomic::Ordering;

use crate::ctx::ExecCtx;
use crate::panic::fatal;

pub use registry::{CodeRegistration, is_registered, register_code};

/// One call site of a registered function: the offsets, from the start of
/// the function's code, of the call's return address and of its handler,
/// and the distance from the frame pointer down to the stack pointer at the
/// call (Cranelift's `frame_offset`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallSite {
    pub ret_offset: u32,
    pub frame_offset: u32,
    pub handler_offset: u32,
}

/// After a guard caught the marker: resumes at the handler of the nearest
/// Cranelift frame above the exit record at `ctx+16`/`ctx+24`. Never
/// returns.
///
/// # Safety
///
/// `ctx` is the running fiber's context, with an unwinding in flight, and
/// its exit record names the frame of generated code that called the
/// guarded entry point through an exit stub. No value with a destructor
/// lives in the frames below that one.
pub(crate) unsafe fn throw_from_guard(ctx: *mut ExecCtx) -> ! {
    // SAFETY: forwarded from the caller.
    unsafe { throw(ctx) }
}

/// From `cheby_rt_panic` and the stack slow paths, called through an exit
/// stub: resumes at the handler of that stub's caller. Never returns.
///
/// # Safety
///
/// As for [`throw_from_guard`].
pub(crate) unsafe fn throw_from_runtime(ctx: *mut ExecCtx) -> ! {
    // SAFETY: forwarded from the caller.
    unsafe { throw(ctx) }
}

/// Resumes the unwinding in flight in the running fiber from a generated
/// cleanup pad, through its exit stub: the pad's frame has no handler at
/// that call, so the walk continues with its caller.
///
/// # Safety
///
/// `ctx` is the running fiber's context, with an unwinding in flight; it
/// is called by generated code only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_rt_unwind_resume(ctx: *mut ExecCtx) {
    // SAFETY: generated code calls it through an exit stub, so the exit
    // record names the resuming frame, and this function holds nothing.
    unsafe { throw(ctx) }
}

/// Walks from the exit record of `ctx` to the nearest handler and jumps
/// there.
///
/// # Safety
///
/// As for [`throw_from_guard`].
unsafe fn throw(ctx: *mut ExecCtx) -> ! {
    // SAFETY: the caller guarantees `ctx` is the running fiber's context.
    let Some(fiber) = (unsafe { ctx.as_ref() }) else {
        fatal(format_args!("an unwinding without a fiber"))
    };
    let fp = fiber.exit_fp.load(Ordering::Relaxed);
    let pc = fiber.exit_pc.load(Ordering::Relaxed);
    // The frames lie above this one. No upper bound: the control block's
    // `stack_top` is only approximate for an isolated entry (a position in
    // its thread's first frame), and the walk reads a frame's record only
    // after its return address was found in generated code.
    let here = std::ptr::from_ref(&fp).addr();
    let top = usize::MAX;
    let found = {
        let tables = registry::tables();
        // SAFETY: the chain starts at a frame of generated code on this
        // stack (the caller's guarantee), and the walk reads the record of
        // a frame only once its return address lies in registered
        // generated code, whose frames keep their records on this stack.
        unsafe { walk::find_handler(&tables, &walk::LiveFrames, fp, pc, here..top) }
        // The registry's lock is released here, before the jump.
    };
    match found {
        // SAFETY: the walk found the handler of a live frame of generated
        // code above this one, with that frame's pointers; the frames
        // below it hold nothing that needs dropping (the caller's
        // guarantee, and this function holds no value with a destructor).
        Ok(handler) => unsafe { arch::resume(handler.pc, handler.sp, handler.fp) },
        Err(error) => fatal(format_args!(
            "{error} (exit record: frame {fp:#x}, return address {pc:#x}; stack {here:#x} to {top:#x})"
        )),
    }
}
