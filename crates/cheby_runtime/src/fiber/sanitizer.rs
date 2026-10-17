//! What the address and thread sanitizers are told about fiber stacks and
//! the runtime's own memory reuse (D-721, D-676).
//!
//! GAP(sanitize-cfg): a sanitizer build passes `-Zsanitizer=<s>` together
//! with `--cfg cheby_sanitize="<s>"`, `"address"` or `"thread"`, declared in
//! the workspace's `check-cfg`. Without that cfg every function here is an
//! `#[inline(always)]` no-op and [`SanFiber`] is zero-sized, so ordinary
//! builds compile to what they did before.
//!
//! GAP(sanitize-annotations): the scheduler switches stacks in two places,
//! a worker resuming a fiber and a fiber parking back to its worker, and
//! both sanitizers keep per-stack state they must be told about:
//!
//! - [`on_create`] (`fiber::create`, before `prepare`): the thread
//!   sanitizer creates the fiber's own sanitizer fiber.
//! - [`before_resume`] (`Worker::run_fiber`, right before the switch): the
//!   address sanitizer starts a switch to the fiber's stack and saves the
//!   worker's fake stack; the thread sanitizer notes the worker's sanitizer
//!   fiber and switches to the fiber's.
//! - [`after_resume`] (`Worker::run_fiber`, after the switch returns): the
//!   address sanitizer finishes the switch back and restores the worker's
//!   fake stack.
//! - [`on_fiber_start`] (`fiber_start`, first): the address sanitizer
//!   finishes the first switch and returns the worker's stack bounds.
//! - [`before_park`] (`park`, right before the switch): the address
//!   sanitizer starts a switch to the worker's stack and saves the fiber's
//!   fake stack, even when the fiber exits; the thread sanitizer switches
//!   to the worker's sanitizer fiber.
//! - [`after_park`] (`park`, after the switch returns): the address
//!   sanitizer finishes the switch and returns the new worker's bounds.
//! - [`on_retire`] (`Worker::retire`, after the worker read the fiber's
//!   last action, before the drop): the address sanitizer frees the
//!   fiber's fake stack; the thread sanitizer destroys its sanitizer fiber.
//!
//! The thread sanitizer switches with flags 0, which synchronizes: the
//! worker and the fiber run one after the other on one thread, and without
//! it the worker's writes before a switch and the fiber's reads after it
//! would be reported as races. A fiber that `Freeze` leaves suspended
//! (D-708) keeps its fake stack and its sanitizer fiber: it is not ending,
//! only never resumed.
//!
//! The other sanitizer-only behavior lives where it applies: `switch`'s
//! `prepare` unpoisons a reused slot before the first frame
//! (GAP(sanitize-slot-unpoison), [`unpoison_stack`]),
//! `panic::stack_position` reads the stack pointer
//! (GAP(sanitize-stack-position)), the block cache poisons the blocks it
//! keeps (GAP(sanitize-block-cache), [`poison`] and [`unpoison`]), and the
//! shared count's last release loads where it would fence
//! (GAP(sanitize-tsan-fences), `rc.rs`).
//!
//! JIT code is not instrumented (D-721). The fields of [`SanFiber`] are
//! atomics used with `Relaxed` only so that the type is `Send` and `Sync`
//! with no `unsafe impl`: like the rest of `FiberSched`, they are used by
//! one thread at a time, the fiber's own while it runs and otherwise the
//! worker that switched away from it or resumes it, with the hand-off
//! ordered by the run queues and the state word.
#![expect(
    clippy::inline_always,
    reason = "the hooks sit on the resume and park paths, and must vanish without the cfg"
)]

#[cfg(any(cheby_sanitize = "address", cheby_sanitize = "thread"))]
use std::ffi::c_void;
#[cfg(any(cheby_sanitize = "address", cheby_sanitize = "thread"))]
use std::sync::atomic::{AtomicPtr, Ordering};

#[cfg(cheby_sanitize = "address")]
use std::sync::atomic::AtomicUsize;

/// The address sanitizer's interface (`<sanitizer/common_interface_defs.h>`,
/// `<sanitizer/asan_interface.h>`), linked from its runtime.
#[cfg(cheby_sanitize = "address")]
mod asan {
    use std::ffi::c_void;

    unsafe extern "C" {
        /// Announces a switch to the stack `[bottom, bottom + size)`. The
        /// current fake stack goes to `*fake_stack_save`, or is destroyed
        /// when `fake_stack_save` is null (the context ends for good).
        pub(super) fn __sanitizer_start_switch_fiber(
            fake_stack_save: *mut *mut c_void,
            bottom: *const c_void,
            size: usize,
        );
        /// Completes a switch on the new stack: installs `fake_stack_save`
        /// (null on a fresh context) and returns the bounds of the stack
        /// left behind.
        pub(super) fn __sanitizer_finish_switch_fiber(
            fake_stack_save: *mut c_void,
            bottom_old: *mut *const c_void,
            size_old: *mut usize,
        );
        /// Marks `[addr, addr + size)` addressable, as a stack frame's
        /// return does; large ranges release their shadow pages instead of
        /// clearing them.
        pub(super) fn __asan_unpoison_stack_memory(addr: usize, size: usize);
        /// Marks `[addr, addr + size)` unaddressable.
        pub(super) fn __asan_poison_memory_region(addr: *const c_void, size: usize);
        /// Marks `[addr, addr + size)` addressable.
        pub(super) fn __asan_unpoison_memory_region(addr: *const c_void, size: usize);
        /// Makes the leak sanitizer scan `[begin, begin + size)` for
        /// pointers, as it scans thread stacks.
        pub(super) fn __lsan_register_root_region(begin: *const c_void, size: usize);
        /// Undoes one `__lsan_register_root_region` of exactly that range.
        pub(super) fn __lsan_unregister_root_region(begin: *const c_void, size: usize);
    }
}

/// The thread sanitizer's fiber interface (`<sanitizer/tsan_interface.h>`),
/// linked from its runtime.
#[cfg(cheby_sanitize = "thread")]
mod tsan {
    use std::ffi::c_void;

    unsafe extern "C" {
        /// The sanitizer fiber running on this thread.
        pub(super) fn __tsan_get_current_fiber() -> *mut c_void;
        /// A new sanitizer fiber.
        pub(super) fn __tsan_create_fiber(flags: u32) -> *mut c_void;
        /// Frees a sanitizer fiber that is not running.
        pub(super) fn __tsan_destroy_fiber(fiber: *mut c_void);
        /// Makes `fiber` the running sanitizer fiber of this thread; flags 0
        /// synchronizes the two.
        pub(super) fn __tsan_switch_to_fiber(fiber: *mut c_void, flags: u32);
    }
}

/// What the sanitizers know about one fiber, kept in its control block's
/// scheduling fields. Zero-sized without the cfg.
#[derive(Debug, Default)]
pub(crate) struct SanFiber {
    /// The fiber's fake stack while it is suspended: written by
    /// the address sanitizer when the fiber parks, installed when it
    /// resumes.
    #[cfg(cheby_sanitize = "address")]
    fake_stack: AtomicPtr<c_void>,
    /// The resuming worker's fake stack while the fiber runs.
    #[cfg(cheby_sanitize = "address")]
    worker_fake_stack: AtomicPtr<c_void>,
    /// The lowest address of the worker stack the fiber was last resumed
    /// from, where it parks back to.
    #[cfg(cheby_sanitize = "address")]
    back_bottom: AtomicUsize,
    /// The size of that worker stack.
    #[cfg(cheby_sanitize = "address")]
    back_size: AtomicUsize,
    /// The fiber's sanitizer fiber, from [`on_create`] to [`on_retire`].
    #[cfg(cheby_sanitize = "thread")]
    tsan_fiber: AtomicPtr<c_void>,
    /// The sanitizer fiber of the worker thread that resumed it last.
    #[cfg(cheby_sanitize = "thread")]
    tsan_back: AtomicPtr<c_void>,
}

impl SanFiber {
    /// A fiber the sanitizers know nothing about yet.
    pub(crate) const fn new() -> Self {
        Self {
            #[cfg(cheby_sanitize = "address")]
            fake_stack: AtomicPtr::new(std::ptr::null_mut()),
            #[cfg(cheby_sanitize = "address")]
            worker_fake_stack: AtomicPtr::new(std::ptr::null_mut()),
            #[cfg(cheby_sanitize = "address")]
            back_bottom: AtomicUsize::new(0),
            #[cfg(cheby_sanitize = "address")]
            back_size: AtomicUsize::new(0),
            #[cfg(cheby_sanitize = "thread")]
            tsan_fiber: AtomicPtr::new(std::ptr::null_mut()),
            #[cfg(cheby_sanitize = "thread")]
            tsan_back: AtomicPtr::new(std::ptr::null_mut()),
        }
    }
}

/// A new scheduled fiber: the thread sanitizer creates its sanitizer
/// fiber.
/// Called by `fiber::create` before `prepare`.
#[inline(always)]
pub(crate) fn on_create(san: &SanFiber) {
    #[cfg(cheby_sanitize = "thread")]
    {
        // SAFETY: creating a TSan fiber has no precondition.
        let fiber = unsafe { tsan::__tsan_create_fiber(0) };
        san.tsan_fiber.store(fiber, Ordering::Relaxed);
    }
    #[cfg(not(cheby_sanitize = "thread"))]
    let _ = san;
}

/// A worker is about to switch to the fiber whose stack is
/// `[slot_lo, slot_lo + slot_len)`. Called by `Worker::run_fiber` right
/// before the switch, with nothing in between that the sanitizers see.
#[inline(always)]
pub(crate) fn before_resume(san: &SanFiber, slot_lo: usize, slot_len: usize) {
    #[cfg(cheby_sanitize = "address")]
    // SAFETY: a switch starts here and `after_resume` finishes it when the
    // fiber switches back; the out-pointer is the fiber's own field, which
    // only this worker uses while the fiber runs.
    unsafe {
        asan::__sanitizer_start_switch_fiber(
            san.worker_fake_stack.as_ptr(),
            std::ptr::with_exposed_provenance(slot_lo),
            slot_len,
        );
    }
    #[cfg(cheby_sanitize = "thread")]
    {
        // SAFETY: reading the running TSan fiber has no precondition.
        let back = unsafe { tsan::__tsan_get_current_fiber() };
        san.tsan_back.store(back, Ordering::Relaxed);
        let fiber = san.tsan_fiber.load(Ordering::Relaxed);
        if !fiber.is_null() {
            // SAFETY: `fiber` is the fiber's live TSan fiber (created by
            // `on_create`, destroyed only when it is retired), and the
            // switch to its stack follows.
            unsafe { tsan::__tsan_switch_to_fiber(fiber, 0) };
        }
    }
    #[cfg(not(any(cheby_sanitize = "address", cheby_sanitize = "thread")))]
    let _ = (san, slot_lo, slot_len);
    #[cfg(cheby_sanitize = "thread")]
    let _ = (slot_lo, slot_len);
}

/// The fiber has switched back to the worker. Called by
/// `Worker::run_fiber` right after the switch returns.
#[inline(always)]
pub(crate) fn after_resume(san: &SanFiber) {
    #[cfg(cheby_sanitize = "address")]
    // SAFETY: finishes the switch the fiber's `before_park` started, on the
    // worker's stack, and reinstalls the fake stack `before_resume` saved.
    unsafe {
        asan::__sanitizer_finish_switch_fiber(
            san.worker_fake_stack.load(Ordering::Relaxed),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
    #[cfg(not(cheby_sanitize = "address"))]
    let _ = san;
}

/// The fiber runs for the first time. Called first thing by `fiber_start`.
#[inline(always)]
pub(crate) fn on_fiber_start(san: &SanFiber) {
    #[cfg(cheby_sanitize = "address")]
    {
        let mut bottom = std::ptr::null::<c_void>();
        let mut size = 0_usize;
        // SAFETY: finishes the switch `before_resume` started; a fresh
        // context has no fake stack.
        unsafe {
            asan::__sanitizer_finish_switch_fiber(
                std::ptr::null_mut(),
                &raw mut bottom,
                &raw mut size,
            );
        }
        san.back_bottom.store(bottom.addr(), Ordering::Relaxed);
        san.back_size.store(size, Ordering::Relaxed);
    }
    #[cfg(not(cheby_sanitize = "address"))]
    let _ = san;
}

/// The fiber is about to switch to its worker; `exiting` when it never
/// runs again (`AfterSwitch::Exit`), not for `Freeze`. Called by `park`
/// right before the switch, with nothing in between that the sanitizers
/// see. Neither sanitizer needs `exiting` today (an exiting fiber's fake
/// stack is freed by [`on_retire`]); plan §2.3's shape keeps it.
#[inline(always)]
pub(crate) fn before_park(san: &SanFiber, exiting: bool) {
    // An exiting fiber keeps its fake stack too: `park` hands the worker
    // its action through the address of a local, which may live there, and
    // the worker reads it after the switch. [`on_retire`] frees it.
    #[cfg(cheby_sanitize = "address")]
    // SAFETY: a switch to the worker's stack, whose bounds the last finish
    // on this thread returned, starts here; the resuming worker's
    // `after_resume` finishes it. The out-pointer is the fiber's own field.
    unsafe {
        asan::__sanitizer_start_switch_fiber(
            san.fake_stack.as_ptr(),
            std::ptr::with_exposed_provenance(san.back_bottom.load(Ordering::Relaxed)),
            san.back_size.load(Ordering::Relaxed),
        );
    }
    #[cfg(cheby_sanitize = "thread")]
    {
        let back = san.tsan_back.load(Ordering::Relaxed);
        if !back.is_null() {
            // SAFETY: `back` is the TSan fiber of the worker thread that
            // resumed this fiber, which the switch below returns to.
            unsafe { tsan::__tsan_switch_to_fiber(back, 0) };
        }
    }
    let _ = exiting;
    #[cfg(not(any(cheby_sanitize = "address", cheby_sanitize = "thread")))]
    let _ = san;
}

/// The fiber has been resumed, maybe by another worker. Called by `park`
/// right after the switch returns.
#[inline(always)]
pub(crate) fn after_park(san: &SanFiber) {
    #[cfg(cheby_sanitize = "address")]
    {
        let mut bottom = std::ptr::null::<c_void>();
        let mut size = 0_usize;
        // SAFETY: finishes the switch the resuming worker's
        // `before_resume` started, reinstalling the fake stack this fiber
        // saved when it parked.
        unsafe {
            asan::__sanitizer_finish_switch_fiber(
                san.fake_stack.load(Ordering::Relaxed),
                &raw mut bottom,
                &raw mut size,
            );
        }
        san.back_bottom.store(bottom.addr(), Ordering::Relaxed);
        san.back_size.store(size, Ordering::Relaxed);
    }
    #[cfg(not(cheby_sanitize = "address"))]
    let _ = san;
}

/// The fiber has ended and is being retired. Called by `Worker::retire`
/// before its control block is dropped.
#[inline(always)]
pub(crate) fn on_retire(san: &SanFiber) {
    #[cfg(cheby_sanitize = "address")]
    {
        let dead = san.fake_stack.swap(std::ptr::null_mut(), Ordering::Relaxed);
        if !dead.is_null() {
            free_fake_stack(dead);
        }
    }
    #[cfg(cheby_sanitize = "thread")]
    {
        let fiber = san.tsan_fiber.swap(std::ptr::null_mut(), Ordering::Relaxed);
        if !fiber.is_null() {
            // SAFETY: the fiber has switched away for good, so its TSan
            // fiber is not running, and it is destroyed once (the field is
            // cleared).
            unsafe { tsan::__tsan_destroy_fiber(fiber) };
        }
    }
    #[cfg(not(any(cheby_sanitize = "address", cheby_sanitize = "thread")))]
    let _ = san;
}

/// Frees `dead`, the fake stack of a fiber that ended, on the worker that
/// retires it. The address sanitizer frees only the fake stack of the
/// context a switch leaves for good, so the worker pretends to switch to
/// the dead fiber's context and away from it, without moving its stack
/// pointer: it installs `dead` in place of its own fake stack, frees it,
/// and reinstalls its own. Nothing runs between the calls but this
/// function's own code; the bounds the first finish returns are the
/// thread's real stack, which the last finish restores.
#[cfg(cheby_sanitize = "address")]
#[inline(never)]
fn free_fake_stack(dead: *mut c_void) {
    let mut own = std::ptr::null_mut::<c_void>();
    let mut bottom = std::ptr::null::<c_void>();
    let mut size = 0_usize;
    // SAFETY: four calls that pair up as two switches on this thread, with
    // no switch in progress before them (the retiring worker runs its own
    // code). The first saves the worker's fake stack in `own`; the first
    // finish installs `dead`, a fake stack no context uses any more, and
    // returns the thread's stack bounds; the second start frees `dead`
    // (null save) and names the same bounds; the last finish reinstalls
    // `own`.
    unsafe {
        asan::__sanitizer_start_switch_fiber(&raw mut own, std::ptr::null(), 0);
        asan::__sanitizer_finish_switch_fiber(dead, &raw mut bottom, &raw mut size);
        asan::__sanitizer_start_switch_fiber(std::ptr::null_mut(), bottom, size);
        asan::__sanitizer_finish_switch_fiber(own, std::ptr::null_mut(), std::ptr::null_mut());
    }
}

/// GAP(sanitize-slot-unpoison): marks the stack `[low, low + len)`
/// addressable before a fresh context's first frame is written. A fiber
/// that exits never returns from its frames, so the redzones
/// the address sanitizer poisoned in them stay in the slot, and the next fiber
/// on it would trip over them. Large ranges cost a release of shadow
/// pages, not a clear.
#[cfg_attr(
    any(windows, not(any(target_arch = "aarch64", target_arch = "x86_64"))),
    expect(
        dead_code,
        reason = "only the supported platforms' `prepare` writes a first frame (D-714)"
    )
)]
#[inline(always)]
pub(crate) fn unpoison_stack(low: usize, len: usize) {
    #[cfg(cheby_sanitize = "address")]
    // SAFETY: changes only AddressSanitizer's shadow of a range the caller
    // owns.
    unsafe {
        asan::__asan_unpoison_stack_memory(low, len);
    }
    #[cfg(not(cheby_sanitize = "address"))]
    let _ = (low, len);
}

/// GAP(sanitize-block-cache): marks the `len` bytes at `block`
/// unaddressable while a free block waits in the block cache, so a use
/// after free into a cached block is reported.
#[inline(always)]
pub(crate) fn poison(block: *const u8, len: usize) {
    #[cfg(cheby_sanitize = "address")]
    // SAFETY: changes only AddressSanitizer's shadow of a block the caller
    // owns and nothing references.
    unsafe {
        asan::__asan_poison_memory_region(block.cast(), len);
    }
    #[cfg(not(cheby_sanitize = "address"))]
    let _ = (block, len);
}

/// Undoes [`poison`] when a cached block is handed out again.
#[inline(always)]
pub(crate) fn unpoison(block: *const u8, len: usize) {
    #[cfg(cheby_sanitize = "address")]
    // SAFETY: changes only AddressSanitizer's shadow of a block the caller
    // owns.
    unsafe {
        asan::__asan_unpoison_memory_region(block.cast(), len);
    }
    #[cfg(not(cheby_sanitize = "address"))]
    let _ = (block, len);
}

/// GAP(sanitize-lsan): makes the leak sanitizer scan the stack slot
/// `[low, low + len)` while a fiber may use it. Fiber stacks are `mmap`'d
/// memory the sanitizer does not scan, so objects held only by a fiber
/// that is parked, blocked in a pool job or frozen by a halt (D-708,
/// D-859) at the process's exit would be reported as leaks (D-863: told to
/// the sanitizer, never suppressed). Called once per hand-out, undone by
/// [`stop_scanning`] with the same range. Address cfg only: the slot keeps
/// its flag only there.
#[cfg(cheby_sanitize = "address")]
#[inline(always)]
pub(crate) fn scan_as_root(low: usize, len: usize) {
    // SAFETY: registers a range of mapped memory (a slot of a region that
    // is never unmapped) for the leak check's scan; it changes nothing.
    unsafe {
        asan::__lsan_register_root_region(std::ptr::with_exposed_provenance(low), len);
    }
}

/// Undoes [`scan_as_root`] for the same range, when the slot goes back to
/// its pool or a worker's cache.
#[cfg(cheby_sanitize = "address")]
#[inline(always)]
pub(crate) fn stop_scanning(low: usize, len: usize) {
    // SAFETY: the range was registered once by `scan_as_root` and not
    // unregistered since (the slot's flag says so).
    unsafe {
        asan::__lsan_unregister_root_region(std::ptr::with_exposed_provenance(low), len);
    }
}
