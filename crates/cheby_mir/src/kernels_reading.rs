//! The std kernels that only read their arguments: a direct call of one
//! passes its counted arguments borrowed, as a Cheby call passes a
//! borrowed parameter, without the owned temporaries of D-525.
//!
//! A direct call of a plain std kernel moves each counted argument into an
//! owned temporary, so that a kernel that updates a unique argument in
//! place sees the count its wrapper gave it (`lower/external.rs`). A kernel
//! that never updates an argument in place, never keeps a reference to one
//! and never returns one has nothing to see in the count, and the move
//! costs a `dup` and a `drop` whenever the value is used after the call.
//! For the kernels listed here the call borrows the arguments themselves,
//! so a parameter they read can be inferred borrowed (D-715).
//!
//! Each entry is a `pub` function of a std module whose direct calls
//! reach its kernel (`DIRECT_MODULES`), and none is in
//! [`crate::kernels_in_place::IN_PLACE_KERNELS`]. A kernel that may return
//! an argument itself, as some `string` kernels return the text when
//! nothing changes (D-577), is not listed. The runtime's test
//! `reading_kernels_only_read` reads this table and calls every kernel
//! with unique and with shared arguments, and checks that no count
//! changed and nothing was written.

/// `(module, function, native symbol)`, sorted by module and function.
pub const READING_KERNELS: &[(&str, &str, &str)] = &[
    ("std::bytes", "get", "cheby_bytes_get"),
    ("std::bytes", "index_of", "cheby_bytes_index_of"),
    ("std::bytes", "length", "cheby_bytes_length"),
    ("std::list", "get", "cheby_list_get"),
    ("std::list", "length", "cheby_list_length"),
    ("std::map", "get", "cheby_map_get"),
    ("std::map", "has_key", "cheby_map_has_key"),
    ("std::map", "is_empty", "cheby_map_is_empty"),
    ("std::map", "size", "cheby_map_size"),
    ("std::set", "contains", "cheby_set_contains"),
    ("std::set", "is_empty", "cheby_set_is_empty"),
    ("std::set", "size", "cheby_set_size"),
    ("std::string", "compare", "cheby_string_compare"),
    ("std::string", "contains", "cheby_string_contains"),
    ("std::string", "ends_with", "cheby_string_ends_with"),
    (
        "std::string",
        "grapheme_length",
        "cheby_string_grapheme_length",
    ),
    ("std::string", "length", "cheby_string_length"),
    ("std::string", "starts_with", "cheby_string_starts_with"),
];

/// Whether `symbol` is the kernel of an entry of [`READING_KERNELS`].
#[must_use]
pub fn only_reads(symbol: &str) -> bool {
    READING_KERNELS.iter().any(|&(_, _, s)| s == symbol)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernels_in_place::IN_PLACE_KERNELS;

    /// The table is sorted without duplicates, and each entry is a `pub`
    /// function of its std module, without bounds, right after the
    /// `@external(native, …)` attribute that names its symbol, and never
    /// an in-place kernel.
    #[test]
    fn the_table_names_native_wrappers_of_std_that_update_nothing() {
        assert!(
            READING_KERNELS
                .windows(2)
                .all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)),
            "{READING_KERNELS:?}"
        );
        let std_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
        for &(module, function, symbol) in READING_KERNELS {
            assert!(
                !IN_PLACE_KERNELS.iter().any(|&(_, _, s)| s == symbol),
                "{symbol} updates in place"
            );
            let file = module.strip_prefix("std::").unwrap();
            let text = std::fs::read_to_string(std_dir.join(format!("{file}.cheby"))).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            let at = lines
                .iter()
                .position(|l| {
                    l.starts_with(&format!("pub fn {function}("))
                        || l.starts_with(&format!("pub fn {function}<"))
                })
                .unwrap_or_else(|| panic!("{module}::{function} is not a pub fn"));
            if let Some(generics) = lines[at]
                .strip_prefix(&format!("pub fn {function}<"))
                .and_then(|rest| rest.split_once('>'))
            {
                assert!(
                    !generics.0.contains(':'),
                    "{module}::{function} has a bound"
                );
            }
            let attrs: Vec<&str> = lines[..at]
                .iter()
                .rev()
                .take_while(|l| l.starts_with('@'))
                .copied()
                .collect();
            let native = format!("@external(native, \"{symbol}\")");
            assert!(
                attrs.contains(&native.as_str()),
                "{module}::{function}: no `{native}` in {attrs:?}"
            );
            assert!(
                !attrs
                    .iter()
                    .any(|a| a.starts_with("@std_context") || a.starts_with("@blocking")),
                "{module}::{function} is no plain kernel"
            );
            assert!(only_reads(symbol));
        }
        assert!(!only_reads("cheby_list_set"));
        assert!(!only_reads("cheby_string_slice"));
    }
}
