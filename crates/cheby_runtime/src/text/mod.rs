//! `std::string` kernels (F2): the 27 `@external(native, …)` functions of
//! `std/string.cheby`, over UTF-8 `String` objects ([`crate::string`]).
//!
//! - `ops`: the semantics on Rust `&str`, with no objects: code-point
//!   counts and positions (D-302), clamping (D-298), splitting (D-315),
//!   padding, trimming (`White_Space`, D-310) and grapheme clusters.
//! - `kernels`: the C entry points. Arguments are borrowed and results
//!   owned (D-187); results are built through [`crate::values`],
//!   [`crate::string`] and `list::from_iter` (plan §4.3.4).
//!
//! In place at count one (D-525, plan §6): `add`, `pad_end`, `trim`,
//! `trim_start`, `trim_end`, `remove_prefix`, `remove_suffix` and `slice`
//! change their text argument in place when its count is exactly one, no
//! other argument is the same object, and (for the appending ones) its
//! capacity suffices, then `dup` it and return it. A unique text that
//! must grow moves into a new object of twice its capacity, so a chain of
//! `+` on a unique accumulator runs in amortized O(1) per byte appended; a
//! shared or static text is copied with twice its length
//! (D-849, refining D-577). Either is at least what the
//! result needs.
//!
//! Unicode (D-310, D-552): case mapping is
//! the Rust standard library's full mapping (`str::to_lowercase`,
//! `str::to_uppercase`, which apply `SpecialCasing.txt`'s unconditional
//! mappings and the language-independent `Final_Sigma` rule), whitespace is
//! `char::is_whitespace` (`White_Space`), and grapheme clusters are
//! `unicode-segmentation`'s extended clusters. A test asserts that both
//! follow the same Unicode version, 17.0.0 today.
//!
//! Every cost of `docs/stdlib/string.md` is required (D-300): kernels read
//! their arguments without re-validating UTF-8 (a `String` object always
//! holds UTF-8), so `starts_with`, `ends_with`, `remove_prefix` and
//! `remove_suffix` are O(m) in the part they look for.

pub(crate) mod kernels;
mod ops;

#[cfg(test)]
mod tests;

/// Appends every `std::string` kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(kernels::all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["string"];

/// The C signatures of the kernels, as the Rust types they take and
/// return, for the cross-check against `std/` in `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    kernels::all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}
