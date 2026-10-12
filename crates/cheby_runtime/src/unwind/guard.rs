//! Guards on the runtime's entry points (D-697, I-3).
//!
//! A Cheby unwinding inside Rust is a Rust unwinding whose payload is the
//! zero-sized [`ChebyUnwind`] marker ([`super::raise`]). It must never
//! reach a Cranelift frame, so every `extern "C"` kernel and helper that
//! may unwind runs its whole body in [`guarded`], which catches the marker
//! at the entry point, after every Rust frame above it ran its `Drop`s
//! (I-4), and hands the unwinding to the backend the fiber's code was
//! compiled for:
//!
//! - `Flag`: sets the fiber's unwind flag and returns the entry point's
//!   [`Unwound::UNWOUND`] sentinel; generated code tests the flag after the
//!   call and runs its cleanup pad.
//! - `TryCall`: resumes at the handler of the nearest Cranelift frame
//!   ([`super::try_call::throw_from_guard`]).
//! - `Off`: never reached, since a raise under `Off` abandons the entry.
//!
//! Any other Rust panic stays fatal, as before the guards: the panic hook
//! has printed it, and the process aborts.
//!
//! A guard hands the unwinding over to generated code right above it, so
//! no runtime code calls another guarded entry point from Rust: under
//! `TryCall` the inner guard would jump over the outer kernel's frames
//! without their drops (board U2b-1). Hosts that run Rust tasks on an
//! entry catch the marker themselves (`entry::catch_unwinding`). `cheby_rt_dup` and
//! `cheby_rt_drop` are not guarded: a panic in a drop function aborts
//! (D-171). The source test `guards_cover_entry_points` keeps every entry
//! point of the crate guarded or on its list of entry points that never
//! unwind.

use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::abi::{UnwindBackend, Word, unwind_flag};
use crate::ctx::ExecCtx;
use crate::panic::fatal;
use crate::rc;

use super::raise::{ChebyUnwind, running};
use super::record::{self, UnwindRecord};

/// What a guarded entry point returns when an unwinding passes through it
/// under the flag backend: one impl per C result type a guarded entry
/// point has. Generated code reads the flag, not the sentinel.
pub(crate) trait Unwound: Sized {
    /// The sentinel: 0, `false`, `()` or `0.0`.
    const UNWOUND: Self;
}

impl Unwound for () {
    const UNWOUND: Self = ();
}

impl Unwound for bool {
    const UNWOUND: Self = false;
}

impl Unwound for f32 {
    const UNWOUND: Self = 0.0;
}

impl Unwound for f64 {
    const UNWOUND: Self = 0.0;
}

/// Integers: 0.
macro_rules! unwound_zero {
    ($($t:ty),*) => {
        $(impl Unwound for $t {
            const UNWOUND: Self = 0;
        })*
    };
}

unwound_zero!(u8, u16, u32, u64, usize, i8, i16, i32, i64);

/// Runs `body`, the whole body of an `extern "C"` kernel or helper that may
/// unwind (D-697). `ctx` is the entry point's own context argument, or
/// `None` for kernels without one (the current fiber is used then).
#[inline]
pub(crate) fn guarded<R: Unwound>(ctx: Option<*mut ExecCtx>, body: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) if payload.is::<ChebyUnwind>() => {
            drop(payload);
            deliver(ctx)
        }
        Err(_) => fatal(format_args!("a Rust panic inside the runtime")),
    }
}

/// Hands the unwinding a guard caught to the backend; returns the
/// sentinel under the flag backend.
#[cold]
#[inline(never)]
fn deliver<R: Unwound>(ctx: Option<*mut ExecCtx>) -> R {
    let Some(ctx) = running(ctx) else {
        fatal(format_args!(
            "an unwinding reached a guard outside of any entry"
        ));
    };
    // SAFETY: `running` returns the running fiber's control block, alive
    // while it runs; only shared references to it are made.
    let fiber = unsafe { &*ctx };
    match fiber.unwind_backend() {
        UnwindBackend::Flag => {
            set_flag(fiber);
            R::UNWOUND
        }
        UnwindBackend::TryCall => {
            set_flag(fiber);
            // SAFETY: `ctx` is the running fiber's, with the unwinding in
            // flight that the guard caught; the Rust frames above the guard
            // have run their drops and the payload is dropped, and the
            // entry point's own frame holds nothing that needs dropping
            // once its body has unwound.
            unsafe { super::try_call::throw_from_guard(ctx) }
        }
        UnwindBackend::Off => fatal(format_args!(
            "an unwinding reached a guard in code compiled without an unwinding backend"
        )),
    }
}

/// Sets the fiber's unwind flag for the record in flight. Under `TryCall`
/// generated code does not read it, but the catch points read and clear
/// it the same way under both backends (board U2b-1).
pub(crate) fn set_flag(fiber: &ExecCtx) {
    let flag = match record::peek_kind(fiber) {
        Some(RecordKind::Cancel) => unwind_flag::CANCEL,
        Some(RecordKind::Panic) | None => unwind_flag::PANIC,
    };
    super::flag::set(fiber, flag);
}

/// The owned arguments of a runtime helper while it may raise (D-698):
/// generated code gives a helper its owned parameters on every path, so a
/// helper that raises before it consumed them releases them, and its
/// caller's cleanup pad does not hold them. Dropped by an unwinding that
/// leaves the scope before [`DropOnUnwind::disarm`], which the helper
/// calls where it takes the words over.
#[derive(Debug)]
pub(crate) struct DropOnUnwind<const N: usize> {
    words: [Word; N],
}

impl<const N: usize> DropOnUnwind<N> {
    /// Holds `words`, each 0, a tagged integer or an owned reference.
    pub(crate) const fn new(words: [Word; N]) -> Self {
        Self { words }
    }

    /// The words held, still owned by the guard.
    pub(crate) const fn words(&self) -> [Word; N] {
        self.words
    }

    /// The helper has taken the words over: nothing is released.
    pub(crate) fn disarm(self) {
        std::mem::forget(self);
    }
}

impl<const N: usize> Drop for DropOnUnwind<N> {
    fn drop(&mut self) {
        for word in self.words {
            // SAFETY: each word is 0, a tagged integer or a reference the
            // helper owns and has not consumed (`new`, `disarm`); `drop`
            // skips 0 and tagged integers.
            unsafe { rc::drop(word) };
        }
    }
}

/// What kind of record is in flight, for the flag's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// A panic.
    Panic,
    /// A cancellation.
    Cancel,
}

impl RecordKind {
    /// The kind of `record`.
    pub(crate) const fn of(record: &UnwindRecord) -> Self {
        match record {
            UnwindRecord::Panic(_) => Self::Panic,
            UnwindRecord::Cancel => Self::Cancel,
        }
    }
}
