//! Starting a fiber (D-666, D-670, D-677, architecture §5.4): the kernels
//! `sys_spawn` and `sys_spawn_with_stack` of `std::fiber`, and the stub of
//! `sys_spawn_detached` (D-754), which package L1r replaces.
//!
//! `sys_spawn_with_stack(owner, stack_bytes, f)` is `sys_spawn` with a
//! stack of `stack_bytes`, clamped to 1 MiB to 16 GiB and rounded up to
//! whole pages (D-659, D-660, D-298); the pool then rounds it up to its
//! class (D-677). `sys_spawn` uses the runtime's default fiber stack, the
//! manifest's `fiber-stack` (D-660, `RuntimeConfig.fiber_stack`).
//!
//! `sys_spawn(owner, f)`, in order:
//!
//! 1. allocates the `Fiber` handle and its join cell, and marks `f` shared,
//!    outside every lock (D-705), so that running out of memory raises
//!    before a member slot is reserved (D-642; review finding MEM-1: a
//!    raise after the reservation left the scope waiting for a fiber that
//!    never existed);
//! 2. joins the scope `owner` under its lock: a scope that has ended makes
//!    the spawn panic with `spawn into a scope that has ended` (D-666,
//!    D-732); otherwise a member slot is reserved, so the scope waits for
//!    the fiber from now on;
//! 3. without a runtime, ends the program (D-692);
//! 4. takes a reference to `f` for the fiber;
//! 5. creates the fiber on a slot of the requested size (D-125, D-659,
//!    D-677), by default the runtime's fiber stack (8 MiB unless
//!    `[runtime] fiber-stack` sets it, D-660),
//!    inheriting the spawner's domain, trampolines, hash seed, entry mode
//!    and unwind-edge setting (D-742); a stack region that cannot be
//!    mapped gives the slot back and panics with `out of memory` (D-677);
//!
//! Every raise here happens with no lock held (plan §0 'Locks'): `join`
//! and `leave` release the scope's lock before they return, and a raise
//! takes the spawner's own responsible scope's lock to record its panic
//! (D-666).
//!
//! 6. records the fiber in its slot, which tells whether the scope has
//!    failed or is being cancelled, read under the scope's lock: then the
//!    fiber gets its target before it is first queued, so it starts
//!    cancelled and never calls `f` (D-666; survey-u5 §2.3, hazard 2: a
//!    walk between the reservation and this step found the slot without
//!    its fiber). A scope whose enclosing scope has failed or is being
//!    cancelled counts as cancelled too, before the walk reaches it, with
//!    the outermost such scope as the target (D-667). A walk after this
//!    step reaches the fiber as a member.
//!    Then it queues the fiber to the worker's runnext slot, so it runs
//!    when the spawner blocks (architecture §3.5). A fiber that never calls
//!    `f` releases it (D-670) and joins as failed (D-666).

use std::sync::Arc;

use super::base::{Base, FiberHandle, JoinCell, fiber_main};
use super::scope::{Membership, state_of};
use super::{Owned, cancel, needs_scheduler, running};
use crate::abi::{PanicKind, Word};
use crate::alloc::OUT_OF_MEMORY;
use crate::ctx::ExecCtx;
use crate::fiber::stack::{MAX_STACK, clamp_user_size};
use crate::handles::{self, HandleKind};
use crate::panic::fatal;
use crate::rc;
use crate::sched::{self, Shared, SpawnError, StartFn};
use crate::unwind::guard::guarded;
use crate::unwind::raise::raise_panic;

/// `fiber::sys_spawn`, private, which `fiber::spawn` calls: starts a fiber
/// in the scope `owner` that calls `f` (the module documentation gives the
/// steps), and returns its `Fiber` handle, owned.
///
/// # Safety
///
/// `ctx` is the running fiber's context, `owner` a live `Scope` handle and
/// `f` a live `fn() -> T` closure, both borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_fiber_sys_spawn(ctx: *mut ExecCtx, owner: Word, f: Word) -> Word {
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        spawn(ctx, owner, f, None, "fiber::spawn")
    })
}

/// The stack of `fiber::spawn_with_stack(owner, stack_bytes, f)` (D-659,
/// D-660, D-677): `stack_bytes` clamped to 1 MiB to 16 GiB, a negative
/// request counting as zero (D-298), then rounded up to a whole number of
/// pages of `page` bytes (a power of two). GAP(runtime-table-bytes): the
/// runtime, not the manifest or std, rounds to its page size.
pub(crate) fn requested_stack_size(stack_bytes: i64, page: usize) -> usize {
    let clamped = clamp_user_size(u64::try_from(stack_bytes).unwrap_or(0));
    let page = page.max(1);
    clamped.div_ceil(page).saturating_mul(page).min(MAX_STACK)
}

/// The size of a page of this platform.
fn page_size() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: `sysconf` has no preconditions.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if let Some(size) = usize::try_from(size)
            .ok()
            .filter(|size| size.is_power_of_two())
        {
            return size;
        }
    }
    4096
}

/// The body of [`cheby_fiber_sys_spawn`] and
/// [`cheby_fiber_sys_spawn_with_stack`]: a stack of `stack_size` bytes, or
/// the runtime's default fiber stack for `None`. `operation` names the std
/// function in the no-runtime error (D-692).
///
/// # Safety
///
/// As for [`cheby_fiber_sys_spawn`].
unsafe fn spawn(
    ctx: *mut ExecCtx,
    owner: Word,
    function: Word,
    stack_size: Option<usize>,
    operation: &str,
) -> Word {
    // SAFETY: forwarded from the caller.
    let me = unsafe { running(ctx) };
    // SAFETY: a live `Scope` handle, borrowed.
    let scope = unsafe { state_of(owner) };
    // 1. The handle and its join cell.
    let cell = Arc::new(JoinCell::default());
    // SAFETY: `ctx` is the running fiber's; a fresh handle, owned here
    // until it is returned.
    let handle = unsafe {
        Owned::new(handles::new_handle(
            ctx,
            HandleKind::Fiber,
            Box::new(FiberHandle(Arc::clone(&cell))),
        ))
    };
    // SAFETY: a live closure, borrowed; every object it reaches with a
    // positive count is confined to this fiber (I-1). If the walk raises,
    // what it marked stays shared together with all it reaches.
    unsafe { rc::mark_shared(function) };
    // 2. Join the scope.
    let slot = match scope.join() {
        Membership::Ended => {
            drop(handle);
            raise_panic(
                Some(ctx),
                PanicKind::Explicit,
                Some("spawn into a scope that has ended"),
            )
        }
        Membership::Member(index) => index,
    };
    // 3. The no-runtime rule.
    let Some(runtime) = me.sched.runtime() else {
        needs_scheduler(operation)
    };
    let stack_size = stack_size.unwrap_or_else(|| default_stack(runtime));
    // 4. Take the fiber's reference to the function, marked shared in
    // step 1.
    // SAFETY: a live closure, borrowed.
    unsafe { rc::dup(function) };
    // 5. Create the fiber.
    let base = Base {
        function,
        cell,
        scope: Arc::clone(&scope),
        slot,
    };
    let start: StartFn = Box::new(move |ctx| {
        // SAFETY: `ctx` is the new fiber's control block, running on this
        // thread, and `base` holds the reference taken above.
        unsafe { fiber_main(ctx, base) }
    });
    match sched::create_fiber(me, stack_size, start) {
        Ok(fiber) => {
            // 6. Record it, give it its target if its scope is being
            // cancelled, and queue it.
            // D-769. An enclosing scope that has failed
            // or is being cancelled cancels this one too, before the walk
            // reaches it (`cancel::check_chain`, review finding
            // CONC-W2-1); the outermost is the target (D-667).
            let failed = scope.fill(slot, fiber);
            let target = cancel::outermost_failing(scope.parent().cloned())
                .or_else(|| failed.then(|| Arc::clone(&scope)));
            if let Some(target) = target {
                // SAFETY: the fiber was just created and is not queued yet,
                // so it cannot have run, let alone retired.
                cancel::set_target(unsafe { fiber.ctx() }, &target);
            }
            sched::schedule_new(fiber, true);
            handle.into_word()
        }
        Err(error) => {
            // The start function was dropped without running: give back
            // the reference it held, and the slot.
            // SAFETY: the reference taken in step 4.
            unsafe { rc::drop(function) };
            scope.leave(slot);
            drop(handle);
            match error {
                SpawnError::NoRuntime => needs_scheduler(operation),
                SpawnError::Stack(_) => {
                    raise_panic(Some(ctx), PanicKind::Explicit, Some(OUT_OF_MEMORY))
                }
            }
        }
    }
}

/// The stack of a fiber spawned without a size: the runtime's default
/// fiber stack (D-660's `fiber-stack`, `RuntimeConfig.fiber_stack`). The
/// pool rounds it up to its class, a whole number of pages (D-677).
fn default_stack(runtime: &Shared) -> usize {
    runtime.fiber_stack()
}

/// `fiber::sys_spawn_with_stack`, private, which `fiber::spawn_with_stack`
/// calls: [`cheby_fiber_sys_spawn`] with a stack of `stack_bytes`, clamped
/// to 1 MiB to 16 GiB and rounded up to whole pages (D-659, D-660,
/// [`requested_stack_size`]). A region that cannot be mapped panics with
/// `out of memory` (D-677); without a runtime the program ends with
/// `fiber::spawn_with_stack needs the fiber scheduler` (D-692).
///
/// # Safety
///
/// `ctx` is the running fiber's context, `owner` a live `Scope` handle and
/// `f` a live `fn() -> T` closure, both borrowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_fiber_sys_spawn_with_stack(
    ctx: *mut ExecCtx,
    owner: Word,
    stack_bytes: i64,
    f: Word,
) -> Word {
    let size = requested_stack_size(stack_bytes, page_size());
    // SAFETY: forwarded from the caller.
    guarded(Some(ctx), || unsafe {
        spawn(ctx, owner, f, Some(size), "fiber::spawn_with_stack")
    })
}

/// `fiber::sys_spawn_detached`, private, which `fiber::spawn_detached`
/// calls. A stub until package L1 (D-754): it ends the program with
/// `fatal runtime error: fiber::spawn_detached is not supported yet`.
///
/// # Safety
///
/// `ctx` is the running fiber's context; `f` is borrowed and not read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cheby_fiber_sys_spawn_detached(ctx: *mut ExecCtx, f: Word) {
    guarded(Some(ctx), || -> () {
        let _ = f;
        fatal(format_args!("fiber::spawn_detached is not supported yet"))
    });
}

#[cfg(test)]
mod tests {
    //! `fiber::spawn_with_stack` and the default fiber stack (D-659,
    //! D-660, D-677): the size as a pure function, and real spawns only at
    //! 1 MiB and 64 MiB (a 16 GiB reservation can fail with `out of memory`
    //! in containers).

    use super::{
        MAX_STACK, cheby_fiber_sys_spawn, cheby_fiber_sys_spawn_with_stack, requested_stack_size,
    };
    use crate::abi::Word;
    use crate::alloc::Domain;
    use crate::ctx::ExecCtx;
    use crate::entry::{
        Callbacks, DEFAULT_STACK_SIZE, EntryConfig, EntryOutcome, STACK_HEADROOM, run_entry,
    };
    use crate::fiber::stack::class_size;
    use crate::fiber_ops::join::cheby_fiber_sys_join;
    use crate::fiber_ops::tests::{close, config, ctx_of, drop_word, int, open};
    use crate::generic::tests::closure;
    use crate::sched::testing::{self, HANG};
    use crate::sched::{Runtime, RuntimeConfig};
    use crate::values;

    const KIB: usize = 1 << 10;
    const MIB: usize = 1 << 20;

    /// A trampoline whose closures return the size of the running fiber's
    /// stack slot: its control block lies at the top of the slot, at most
    /// 64 KiB below it (`MAX_RESERVE`), and its hard limit is
    /// [`STACK_HEADROOM`] above the slot's bottom, so the distance rounds
    /// up to the slot's class, a power of two of at least 512 KiB.
    unsafe extern "C" fn slot_size_t0(ctx: *mut ExecCtx, _function: u64) -> u64 {
        // SAFETY: the running fiber's control block.
        let me = unsafe { &*ctx };
        let top = ctx.addr();
        let size = top
            .wrapping_sub(me.hard_limit())
            .wrapping_add(STACK_HEADROOM)
            .next_power_of_two();
        values::int_word(i64::try_from(size).unwrap_or(-1))
    }

    /// On a runtime of one worker whose default fiber stack is
    /// `fiber_stack`, spawns a fiber for each request, with
    /// `spawn_with_stack(owner, stack_bytes, f)` for `Some(stack_bytes)`
    /// and with `spawn(owner, f)` for `None`, and returns the size of each
    /// one's slot, after checking that nothing leaked.
    fn slot_sizes(fiber_stack: usize, requests: &[Option<i64>]) -> Vec<usize> {
        let requests = requests.to_vec();
        let runtime = Runtime::new(&RuntimeConfig {
            threads: Some(1),
            fiber_stack,
            ..RuntimeConfig::default()
        })
        .unwrap();
        let domain = Domain::new();
        let config = EntryConfig {
            callbacks: Callbacks {
                t0: Some(slot_size_t0),
                ..Callbacks::default()
            },
            ..config(&domain)
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let outcome = testing::start_root_with(&runtime, &config, move |me| {
            let scope = open(me);
            let function = closure(&[values::int_word(0)]);
            let mut sizes = Vec::new();
            for request in requests {
                // SAFETY: the running fiber's context, a live scope and
                // closure, borrowed.
                let fiber: Word = unsafe {
                    match request {
                        Some(bytes) => {
                            cheby_fiber_sys_spawn_with_stack(ctx_of(me), scope, bytes, function)
                        }
                        None => cheby_fiber_sys_spawn(ctx_of(me), scope, function),
                    }
                };
                // SAFETY: a live `Fiber` handle, borrowed.
                let size = int(unsafe { cheby_fiber_sys_join(ctx_of(me), fiber) });
                drop_word(fiber);
                sizes.push(usize::try_from(size).unwrap());
            }
            drop_word(function);
            assert!(close(me, scope));
            tx.send(sizes).unwrap();
            values::nil()
        })
        .recv_timeout(HANG)
        .expect("the root fiber hung");
        runtime.shutdown();
        assert!(
            matches!(outcome, EntryOutcome::Returned(_)),
            "the root did not return: {outcome:?}"
        );
        assert_eq!(domain.live_objects(), 0, "objects leaked");
        rx.recv().unwrap()
    }

    /// D-659, D-660, D-298: the request is clamped to 1 MiB to 16 GiB, a
    /// negative one counting as zero, and rounded up to whole pages.
    #[test]
    fn spawn_with_stack_clamps_and_rounds_to_pages() {
        for page in [4 * KIB, 16 * KIB] {
            assert_eq!(requested_stack_size(0, page), MIB);
            assert_eq!(requested_stack_size(-1, page), MIB);
            assert_eq!(requested_stack_size(i64::MIN, page), MIB);
            assert_eq!(requested_stack_size(512 << 10, page), MIB);
            assert_eq!(requested_stack_size(1 << 20, page), MIB);
            assert_eq!(requested_stack_size(1 << 40, page), 16 << 30);
            assert_eq!(requested_stack_size(i64::MAX, page), MAX_STACK);
            assert_eq!(requested_stack_size((16 << 30) + 1, page), 16 << 30);
            // An odd size rounds up to whole pages.
            assert_eq!(requested_stack_size((1 << 20) + 1, page), MIB + page);
            assert_eq!(
                requested_stack_size((3 << 20) + 12_345, page),
                (3 * MIB + 12_345).div_ceil(page) * page
            );
            assert_eq!(requested_stack_size(64 << 20, page), 64 * MIB);
        }
        assert_eq!(
            requested_stack_size((5 << 20) + 7, 16 * KIB) % (16 * KIB),
            0
        );
        assert_eq!(requested_stack_size((5 << 20) + 7, 4 * KIB) % (4 * KIB), 0);
    }

    /// D-659: requests of 1 MiB and below get a slot of 1 MiB, never one of
    /// the internal 512 KiB class, whatever the runtime's default.
    #[test]
    #[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
    fn spawn_with_stack_of_one_mib_and_less() {
        let sizes = slot_sizes(
            64 * MIB,
            &[Some(1 << 20), Some(0), Some(-1), Some(512 << 10)],
        );
        assert_eq!(sizes, [MIB; 4]);
    }

    /// D-659, D-677: a request of 64 MiB gets a slot of that class, larger
    /// than the runtime's default.
    #[test]
    #[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
    fn spawn_with_stack_of_64_mib() {
        let sizes = slot_sizes(MIB, &[Some(64 << 20)]);
        assert_eq!(sizes, [64 * MIB]);
        assert!(sizes.iter().all(|&size| size >= 64 << 20));
        assert_eq!(class_size(64 * MIB), Some(64 * MIB));
    }

    /// An isolated entry (no runtime) that opens a scope and calls
    /// `spawn_with_stack` in it.
    unsafe extern "C" fn isolated_spawn_with_stack(ctx: *mut ExecCtx) -> u64 {
        // SAFETY: the entry's context.
        let me = unsafe { &*ctx };
        let scope = open(me);
        let function = closure(&[values::int_word(0)]);
        // SAFETY: the entry's context, a live scope and closure, borrowed.
        unsafe { cheby_fiber_sys_spawn_with_stack(ctx, scope, 1 << 20, function) }
    }

    /// D-692: without a runtime, `spawn_with_stack` ends the program with
    /// a fatal error naming it. Runs itself in a child process.
    #[test]
    fn spawn_with_stack_without_a_runtime_needs_the_scheduler() {
        const CHILD: &str = "CHEBY_RT_TEST_SPAWN_WITH_STACK_ISOLATED";
        const NAME: &str =
            "fiber_ops::spawn::tests::spawn_with_stack_without_a_runtime_needs_the_scheduler";
        if std::env::var_os(CHILD).is_some() {
            let domain = Domain::new();
            // SAFETY: a test thunk with the entry ABI.
            let outcome = unsafe { run_entry(isolated_spawn_with_stack, &config(&domain)) };
            panic!("the isolated entry ended: {outcome:?}");
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{stderr}");
        assert!(
            stderr.contains(
                "fatal runtime error: fiber::spawn_with_stack needs the fiber scheduler, \
                 which this entry does not run\n"
            ),
            "{stderr}"
        );
    }

    /// D-660: `spawn` takes its stack from `RuntimeConfig.fiber_stack`, the
    /// manifest's `fiber-stack`, which is 8 MiB by default.
    #[test]
    #[cfg_attr(windows, ignore = "fibers on Windows are M6 (D-545, D-714)")]
    fn spawn_with_stack_absent_uses_the_runtimes_fiber_stack() {
        assert_eq!(RuntimeConfig::default().fiber_stack, DEFAULT_STACK_SIZE);
        assert_eq!(DEFAULT_STACK_SIZE, 8 * MIB);
        assert_eq!(slot_sizes(MIB, &[None, None]), [MIB; 2]);
        assert_eq!(slot_sizes(64 * MIB, &[None]), [64 * MIB]);
    }
}
