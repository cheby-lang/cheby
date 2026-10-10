//! Hash array mapped tries for `std::map` and `std::set` (F4, D-039,
//! D-093, D-538). A placeholder until W3-C fills it with the node kinds
//! (layout ids 32-39), the kernels and the API of plan §4.3.5
//! (`for_each_entry`, `rebuild_with_seed`, `seed_of`).

/// Appends every `std::map` and `std::set` kernel symbol and its address to
/// `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    let _ = out;
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &[];
