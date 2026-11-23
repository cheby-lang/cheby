//! The slow paths of the inline allocation path and of inline sized frees
//! (`abi::block_cache`), and the cold drops and counts on the paths of a
//! reuse, as stubs that save every register.
//!
//! Generated code allocates a block of a cached size class and frees one of
//! a known size without a call while the fiber's inline allocation record
//! serves it. What it does not serve, an empty or full class, a budget,
//! counting and the rest, calls the stubs here. Like the entry check's slow
//! paths (D-1775), each stub pushes a frame record, saves every register
//! that a C call may clobber, the full vector registers included
//! (`crate::panic::save_all_aarch64`), calls its Rust body and loads them
//! back, so generated code calls it with Cranelift's `preserve_all`
//! convention and the values live across the allocation, `Float`s
//! included, stay in registers. That convention returns nothing, so an
//! allocation's body leaves its result, the object or 0 when it raised,
//! in the control block (`abi::CTX_ALLOC_RESULT`), where generated code
//! reads it. Under Miri and on Windows the stubs are plain functions, and
//! generated code there calls the C-convention helpers instead.
//!
//! The bodies run the helpers generated code called before
//! (`cheby_rt_alloc`, `cheby_rt_alloc_miss`, `cheby_rt_free_unique`,
//! `cheby_rt_drop`, the count of a reuse hit), so what a program observes
//! is unchanged: the allocation raises in its guard (D-642, D-697), the
//! frees never raise (D-171). An out-of-memory panic's backtrace has no
//! start frame, as before (D-1040).

use crate::abi::Word;
use crate::ctx::ExecCtx;
use crate::panic::fatal;
#[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
use crate::panic::{restore_all_aarch64, save_all_aarch64};
#[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
use crate::panic::{restore_all_x86_64, save_all_x86_64};

/// Defines the stub `$name`, which saves every register around a call of
/// `$body` with its arguments unchanged: AAPCS64 and System V x86-64
/// naked stubs, and a plain function under Miri and on Windows.
macro_rules! saving_stub {
    ($(#[$doc:meta])* $name:ident($($arg:ident: $ty:ty),*) => $body:ident) => {
        $(#[$doc])*
        ///
        /// On AAPCS64 it pushes a frame record (`x29`, `x30`), saves the
        /// registers below it ([`save_all_aarch64`]) and calls the body with
        /// the arguments still in their registers.
        ///
        /// # Safety
        ///
        /// As for the body.
        #[cfg(all(target_arch = "aarch64", not(miri), not(windows)))]
        #[unsafe(naked)]
        #[unsafe(no_mangle)]
        pub(crate) unsafe extern "C" fn $name($($arg: $ty),*) {
            core::arch::naked_asm!(
                "stp x29, x30, [sp, #-16]!",
                "mov x29, sp",
                save_all_aarch64!(),
                "bl {body}",
                restore_all_aarch64!(),
                "ldp x29, x30, [sp], #16",
                "ret",
                body = sym $body,
            )
        }

        $(#[$doc])*
        ///
        /// On System V x86-64 it pushes `rbp` as a frame record, saves the
        /// registers below it ([`save_all_x86_64`]), which keeps `rsp`
        /// 16-byte aligned, and calls the body with the arguments still in
        /// their registers.
        ///
        /// # Safety
        ///
        /// As for the body.
        #[cfg(all(target_arch = "x86_64", not(miri), not(windows)))]
        #[unsafe(naked)]
        #[unsafe(no_mangle)]
        pub(crate) unsafe extern "C" fn $name($($arg: $ty),*) {
            core::arch::naked_asm!(
                "push rbp",
                "mov rbp, rsp",
                save_all_x86_64!(),
                "call {body}",
                restore_all_x86_64!(),
                "pop rbp",
                "ret",
                body = sym $body,
            )
        }

        $(#[$doc])*
        ///
        /// A plain function under Miri and on Windows until M6 (D-1040).
        ///
        /// # Safety
        ///
        /// As for the body.
        #[cfg(not(all(
            any(target_arch = "aarch64", target_arch = "x86_64"),
            not(miri),
            not(windows)
        )))]
        #[unsafe(no_mangle)]
        pub(crate) unsafe extern "C" fn $name($($arg: $ty),*) {
            // SAFETY: forwarded from the caller.
            unsafe { $body($($arg),*) }
        }
    };
}

saving_stub! {
    /// `cheby_rt_alloc_slow(ctx, size, header)`: an allocation the inline
    /// path did not serve. Allocates as `cheby_rt_alloc` does and leaves
    /// the object, or 0 when the request raised, at
    /// `abi::CTX_ALLOC_RESULT`.
    cheby_rt_alloc_slow(ctx: *mut ExecCtx, size: u64, header: u64) => alloc_slow_from
}

saving_stub! {
    /// `cheby_rt_alloc_miss_slow(ctx, size, header)`: [`cheby_rt_alloc_slow`]
    /// for an `alloc_reuse` whose token is 0, which counts the miss as
    /// `cheby_rt_alloc_miss` does.
    cheby_rt_alloc_miss_slow(ctx: *mut ExecCtx, size: u64, header: u64) => alloc_miss_slow_from
}

saving_stub! {
    /// `cheby_rt_free_unique_slow(ctx, obj, size)`: a sized free the inline
    /// path did not serve, as `cheby_rt_free_unique` frees.
    cheby_rt_free_unique_slow(ctx: *mut ExecCtx, obj: Word, size: u64) => free_unique_slow_from
}

saving_stub! {
    /// `cheby_rt_drop_saving(ctx, value)`: `cheby_rt_drop`, for the cold
    /// drop of a shared object on the paths of a reuse and of a drop
    /// specialized by constructor.
    cheby_rt_drop_saving(ctx: *mut ExecCtx, value: Word) => drop_saving_from
}

saving_stub! {
    /// `cheby_rt_reuse_hit()`: counts one reuse hit (`CHEBY_RT_STATS`'s
    /// `reuse hits`) in the current accounting domain when events are
    /// counted. Generated code calls it out of line, only while the
    /// counting flag is not settled to off.
    cheby_rt_reuse_hit() => reuse_hit_from
}

/// Leaves `word` for generated code at `abi::CTX_ALLOC_RESULT` of `ctx`.
fn leave(ctx: *mut ExecCtx, word: Word) {
    // SAFETY: generated code passes the running fiber's control block.
    let Some(fiber) = (unsafe { ctx.as_ref() }) else {
        fatal(format_args!(
            "an allocation's slow path ran without an execution context"
        ));
    };
    fiber.set_alloc_result(word);
}

/// The body of [`cheby_rt_alloc_slow`].
///
/// # Safety
///
/// As for `cheby_rt_alloc`, with a non-null `ctx`.
unsafe extern "C" fn alloc_slow_from(ctx: *mut ExecCtx, size: u64, header: u64) {
    // SAFETY: forwarded from the caller.
    let word = unsafe { super::cheby_rt_alloc(ctx, size, header) };
    leave(ctx, word);
}

/// The body of [`cheby_rt_alloc_miss_slow`].
///
/// # Safety
///
/// As for `cheby_rt_alloc`, with a non-null `ctx`.
unsafe extern "C" fn alloc_miss_slow_from(ctx: *mut ExecCtx, size: u64, header: u64) {
    // SAFETY: forwarded from the caller.
    let word = unsafe { crate::reuse::cheby_rt_alloc_miss(ctx, size, header) };
    leave(ctx, word);
}

/// The body of [`cheby_rt_free_unique_slow`].
///
/// # Safety
///
/// As for `cheby_rt_free_unique`.
unsafe extern "C" fn free_unique_slow_from(ctx: *mut ExecCtx, obj: Word, size: u64) {
    // SAFETY: forwarded from the caller.
    unsafe { crate::reuse::cheby_rt_free_unique(ctx, obj, size) };
}

/// The body of [`cheby_rt_drop_saving`].
///
/// # Safety
///
/// As for `cheby_rt_drop`.
unsafe extern "C" fn drop_saving_from(ctx: *mut ExecCtx, value: Word) {
    // SAFETY: forwarded from the caller.
    unsafe { crate::rc::cheby_rt_drop(ctx, value) };
}

/// The body of [`cheby_rt_reuse_hit`].
extern "C" fn reuse_hit_from() {
    crate::stats::record(crate::stats::Event::ReuseHit);
}
