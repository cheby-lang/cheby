//! `std::string` kernels (F2): code points, graphemes, case mapping,
//! searching and slicing over UTF-8 `String` objects. A placeholder until
//! W2-E fills it; `crate::string` holds the object layout and the
//! uniqueness-aware growth these kernels build on.

/// Appends every `std::string` kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    let _ = out;
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &[];
