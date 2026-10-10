//! `std::env` kernels (D-347): the program's arguments and name, and
//! environment variables. A placeholder until W2-E fills it over
//! `crate::entry::program_args` and `program_name`.

/// Appends every `std::env` kernel symbol and its address to `out`.
pub(crate) fn kernels(out: &mut Vec<(&'static str, *const u8)>) {
    let _ = out;
}

/// The std modules whose every kernel is registered here.
pub(crate) const COMPLETE: &[&str] = &[];
