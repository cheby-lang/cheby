//! The kernels of children where no child starts: on Windows until M6
//! (D-1919, D-983). Each ends the program with a fatal runtime error that
//! names its operation, as `std::net`'s do there; the names and signatures
//! are `work.rs`'s, so `process.rs`'s kernels forward to either.

use super::StartArgs;
use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::fiber_ops::needs_scheduler;

/// `sys_start`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn start(_: *mut ExecCtx, _: &StartArgs) -> Word {
    needs_scheduler("process::start")
}

/// `sys_wait`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn wait(_: *mut ExecCtx, _: Word) -> Word {
    needs_scheduler("process::wait")
}

/// `sys_signal`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn signal(_: *mut ExecCtx, _: Word, _: i64) -> Word {
    needs_scheduler("process::signal")
}

/// `sys_stdin`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn stdin(_: *mut ExecCtx, _: Word) -> Word {
    needs_scheduler("process::stdin")
}

/// `sys_output`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn output(_: *mut ExecCtx, _: Word, _: i64) -> Word {
    needs_scheduler("process::output")
}

/// `sys_read_line`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn read_line(_: *mut ExecCtx, _: Word) -> Word {
    needs_scheduler("process::read_line")
}

/// `sys_read_bytes`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn read_bytes(_: *mut ExecCtx, _: Word, _: i64) -> Word {
    needs_scheduler("process::read_bytes")
}

/// `sys_read_all`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn read_all(_: *mut ExecCtx, _: Word) -> Word {
    needs_scheduler("process::read_all")
}

/// `sys_write_bytes`'s and `sys_write`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn write(_: *mut ExecCtx, _: Word, _: Word, operation: &str) -> Word {
    needs_scheduler(operation)
}

/// `sys_close`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn close(_: *mut ExecCtx, _: Word) -> Word {
    needs_scheduler("process::close")
}

/// `sys_finish`'s body here.
///
/// # Safety
///
/// None: it reads nothing.
pub(crate) unsafe fn finish(_: *mut ExecCtx, _: Word) {
    needs_scheduler("process::finish")
}
