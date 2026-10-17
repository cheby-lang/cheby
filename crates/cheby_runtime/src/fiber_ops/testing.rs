//! Test hooks of cancellation for tests outside `fiber_ops` (plan §2.2):
//! C1b's `entry_slow` tests in wave 3 use them, and V3 keeps them.

use crate::ctx::ExecCtx;
use crate::panic::fatal;

// The hook is part of the crate's test API whether or not a test of this
// crate calls it yet.
const _: fn(&ExecCtx) = cancel_running_fiber_at_next_entry;

/// Sets the running fiber's cancellation target and requests CANCEL, as the
/// walk does, so that its next entry check delivers a cancellation. For
/// tests outside `fiber_ops` (C1b's `entry_slow` tests in wave 3; V3 keeps
/// it).
///
/// The target is the fiber's responsible scope: its innermost open scope,
/// else the scope it was spawned into. Call it inside a scope, whose close
/// catches the cancellation; that scope has no first cause, so it fails as
/// an expired limit does (kind 1, `cheby_rt_scope_close`). A fiber outside
/// every scope has nothing that would catch a cancellation, so the hook
/// ends the process there instead of letting it reach an entry's catch
/// point; a root's base scope (D-859) counts as no scope here, since only
/// a deadlock's cancellation targets it.
pub(crate) fn cancel_running_fiber_at_next_entry(ctx: &ExecCtx) {
    let scope = ctx
        .conc
        .responsible_scope()
        .filter(|scope| !scope.is_root_base());
    let Some(scope) = scope else {
        fatal(format_args!(
            "cancel_running_fiber_at_next_entry needs a fiber inside a scope"
        ));
    };
    super::cancel::set_target(ctx, &scope);
}
