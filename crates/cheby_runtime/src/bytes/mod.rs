//! `std::bytes` kernels (F2, D-095, D-316, D-334): the 36
//! `@external(native, …)` functions of `std/bytes.cheby`, over `Bytes`
//! objects. A `Bytes` has the shape of a `String` with layout id 2
//! (D-454): `len`, `cap`, then the bytes; [`crate::string`] holds the
//! layout and the uniqueness-aware growth.
//!
//! - `kernels`: the C entry points. Arguments are borrowed and results
//!   owned (D-187); results are built through [`crate::values`],
//!   [`crate::string`] and `list::from_iter` (plan §4.3.4).
//!
//! In place at count one (D-316, D-525, plan §6): `push`, `append`,
//! `push_i8` and the `push_<int>_<order>` kernels add to their `Bytes`
//! argument in place when its count is exactly one, it has room, and no
//! other argument is the same object (D-563's alias rule), then `dup` it
//! and return it. A value that must grow is copied into an object of
//! doubled capacity, so a chain of pushes on a unique value runs in
//! amortized O(1) per byte. `slice` keeps its range in place at count
//! one, and returns the argument itself, shared, for the whole range.
//!
//! Positions (D-318, D-334): `get` and the reads give `None` for a
//! negative offset or one whose bytes pass the end; `slice` clamps both
//! bounds to `0..=length` and is empty when `end` is not above `start`
//! (D-298). Equality, hashing and the debug text `bytes::from_list([0x66,
//! 0x6f])` (D-388) are the generic operations of [`crate::generic`].

pub(crate) mod kernels;

#[cfg(test)]
mod tests;

/// Appends every `std::bytes` kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    out.extend(kernels::all().iter().map(|kernel| (kernel.0, kernel.1)));
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &["bytes"];

/// The C signatures of the kernels, as the Rust types they take and
/// return, for the cross-check against `std/` in `symbols.rs`.
#[cfg(test)]
pub(crate) fn signatures() -> Vec<(&'static str, &'static [&'static str], &'static str)> {
    kernels::all()
        .iter()
        .map(|kernel| (kernel.0, kernel.2, kernel.3))
        .collect()
}
