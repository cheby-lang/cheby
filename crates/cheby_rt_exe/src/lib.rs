//! The runtime archive of executables (D-928): `cheby_runtime` with
//! mimalloc as the global allocator (D-890), built as the staticlib
//! `libcheby_rt_exe.a` that `cheby build` links (D-929). It holds nothing
//! else: `cheby_rt_main`, the image installer and the image format live in
//! `cheby_runtime`, so the runtime ID covers them (D-930).

/// The global allocator of executables, as `cheby_cli` sets it for
/// `cheby run` (D-890). Sanitizer builds keep the system allocator, which
/// the sanitizers intercept (D-721, D-875).
#[cfg(not(any(cheby_sanitize = "address", cheby_sanitize = "thread")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Every exported symbol of the runtime, so the archive holds them all.
pub use cheby_runtime;
