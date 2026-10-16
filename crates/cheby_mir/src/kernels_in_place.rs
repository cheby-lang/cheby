//! The std kernels that update their first argument in place when it is
//! unique at run time (D-525), for the kernel lines of `--reuse-report`
//! (D-717, D-852).
//!
//! A call of one of these functions whose first argument is a local at its
//! last use gets a line saying that the update is in place when that value
//! is unique at run time. Every entry is a `pub` function of `std` with an
//! `@external(native, …)` wrapper (D-455), whose native kernel writes into
//! a unique first argument: the list root updates (F3), the map and set
//! updates (§9.3), and the `String` and `Bytes` appends (D-316, D-577).
//! None has a bound, so the first argument of a call is the first operand
//! (D-453 puts dictionaries first). A test reads each module's source and
//! checks the wrapper and its symbol.

/// `(module, function, native symbol)`, sorted by module and function.
pub const IN_PLACE_KERNELS: &[(&str, &str, &str)] = &[
    ("std::bytes", "append", "cheby_bytes_append"),
    ("std::bytes", "push", "cheby_bytes_push"),
    ("std::bytes", "push_i16_be", "cheby_bytes_push_i16_be"),
    ("std::bytes", "push_i16_le", "cheby_bytes_push_i16_le"),
    ("std::bytes", "push_i32_be", "cheby_bytes_push_i32_be"),
    ("std::bytes", "push_i32_le", "cheby_bytes_push_i32_le"),
    ("std::bytes", "push_i64_be", "cheby_bytes_push_i64_be"),
    ("std::bytes", "push_i64_le", "cheby_bytes_push_i64_le"),
    ("std::bytes", "push_i8", "cheby_bytes_push_i8"),
    ("std::bytes", "push_u16_be", "cheby_bytes_push_u16_be"),
    ("std::bytes", "push_u16_le", "cheby_bytes_push_u16_le"),
    ("std::bytes", "push_u32_be", "cheby_bytes_push_u32_be"),
    ("std::bytes", "push_u32_le", "cheby_bytes_push_u32_le"),
    ("std::bytes", "push_u64_be", "cheby_bytes_push_u64_be"),
    ("std::bytes", "push_u64_le", "cheby_bytes_push_u64_le"),
    ("std::list", "append", "cheby_list_append"),
    ("std::list", "prepend", "cheby_list_prepend"),
    ("std::list", "push", "cheby_list_push"),
    ("std::list", "set", "cheby_list_set"),
    ("std::map", "insert", "cheby_map_insert"),
    ("std::map", "remove", "cheby_map_remove"),
    ("std::map", "upsert", "cheby_map_upsert"),
    ("std::set", "insert", "cheby_set_insert"),
    ("std::set", "remove", "cheby_set_remove"),
    ("std::string", "add", "cheby_string_add"),
];

/// Whether `module::function` (as `std::set` and `insert`) is a kernel of
/// [`IN_PLACE_KERNELS`].
#[must_use]
pub fn updates_in_place(module: &str, function: &str) -> bool {
    IN_PLACE_KERNELS
        .binary_search_by(|&(m, f, _)| (m, f).cmp(&(module, function)))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is sorted without duplicates, and each entry is a `pub`
    /// function of its std module, without bounds, right after the
    /// `@external(native, …)` attribute that names its symbol (and any
    /// other `@external` lines).
    #[test]
    fn the_table_names_native_wrappers_of_std() {
        assert!(
            IN_PLACE_KERNELS
                .windows(2)
                .all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)),
            "{IN_PLACE_KERNELS:?}"
        );
        let std_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
        for &(module, function, symbol) in IN_PLACE_KERNELS {
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
            let head = lines[at];
            if let Some(generics) = head
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
            assert!(updates_in_place(module, function));
        }
        assert!(!updates_in_place("std::io", "println"));
        assert!(!updates_in_place("std::set", "contains"));
        assert!(!updates_in_place("app", "insert"));
    }
}
