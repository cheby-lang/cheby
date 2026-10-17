//! Allocations the interners leak on purpose, announced to the leak
//! sanitizer.
//!
//! [`crate::name::Interner`] and [`crate::ty::TyInterner`] leak every entry
//! so that lookups return `&'static` data: every command is its own process
//! (D-268). When an interner's tables are dropped, nothing points at those
//! entries any more, so in a build with `--cfg cheby_sanitize="address"`
//! the leak sanitizer would report every interned name and type at the
//! exit of every `cheby` process (D-721's address sanitizer job). D-863 asks
//! that behavior made intentional be told to the sanitizer, never
//! suppressed, so each leaked entry is marked as one the checker ignores;
//! what it owns (a type's argument list) stays reachable through it
//! (GAP(sanitize-lsan)). Without the cfg this is a no-op.

/// Marks `leaked`, an allocation leaked on purpose, as no leak for the leak
/// sanitizer. A zero-sized value owns no allocation and is skipped.
#[inline]
pub(crate) fn intended<T: ?Sized>(leaked: &'static T) {
    #[cfg(cheby_sanitize = "address")]
    if std::mem::size_of_val(leaked) != 0 {
        // SAFETY: `__lsan_ignore_object` only reads the allocator's
        // metadata for the heap block that starts at the pointer, and
        // `leaked` is the start of a live `Box` allocation that is never
        // freed.
        unsafe { lsan::__lsan_ignore_object(std::ptr::from_ref(leaked).cast()) };
    }
    #[cfg(not(cheby_sanitize = "address"))]
    let _ = leaked;
}

/// The leak sanitizer's interface (`<sanitizer/lsan_interface.h>`), linked
/// from the address sanitizer's runtime.
#[cfg(cheby_sanitize = "address")]
mod lsan {
    use std::ffi::c_void;

    unsafe extern "C" {
        /// Makes the leak checker treat the heap block at `p` as reachable,
        /// with everything it points to.
        pub(super) fn __lsan_ignore_object(p: *const c_void);
    }
}
