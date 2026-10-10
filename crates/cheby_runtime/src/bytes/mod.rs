//! `std::bytes` kernels (F2, D-095, D-316, D-334): byte sequences that
//! update in place at count one. A placeholder until W4-D fills it;
//! `crate::string` holds the object layout (shared with `String`) and the
//! uniqueness-aware growth.

/// Appends every `std::bytes` kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    let _ = out;
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &[];
