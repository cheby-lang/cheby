//! The std waits whose tail calls keep their caller's frame (D-861).
//!
//! A tail call from a function outside package `std` to one of these
//! functions is lowered as a call followed by a return, so the caller's
//! frame stays on the stack while the fiber waits and a deadlock report can
//! name the caller's call (D-856). Each never calls a function value, so the
//! change adds at most one frame per waiting fiber and no loop can grow the
//! stack through it. `selector::select` is not here: it calls the chosen
//! arm's function in tail position, and select-driven loops rely on that. A
//! test reads each module's source and checks that the function exists and
//! calls no function value.

/// `(module, function)`, sorted by module and function.
pub const FRAME_KEEPING_WAITS: &[(&str, &str)] = &[
    ("std::channel", "receive"),
    ("std::channel", "send"),
    ("std::fiber", "join"),
];

/// Whether `module::function` (as `std::channel` and `receive`) is a wait
/// of [`FRAME_KEEPING_WAITS`].
#[must_use]
pub fn keeps_frame(module: &str, function: &str) -> bool {
    FRAME_KEEPING_WAITS
        .binary_search_by(|&(m, f)| (m, f).cmp(&(module, function)))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `@external` functions declared in `text`, a std module's source.
    fn externals(text: &str) -> Vec<String> {
        let lines: Vec<&str> = text.lines().collect();
        let mut out = Vec::new();
        for (at, line) in lines.iter().enumerate() {
            let Some(rest) = line
                .strip_prefix("pub fn ")
                .or_else(|| line.strip_prefix("fn "))
            else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let external = lines[..at]
                .iter()
                .rev()
                .take_while(|l| l.starts_with('@'))
                .any(|l| l.starts_with("@external(native,"));
            if external {
                out.push(name);
            }
        }
        out
    }

    /// The table is sorted without duplicates, and each entry is a `pub`
    /// function of its std module that takes no function value and calls
    /// only the module's `@external(native, …)` functions: a native
    /// wrapper itself (no body), or a body whose every call is one.
    #[test]
    fn the_waits_exist_in_std_and_call_no_function_value() {
        assert!(
            FRAME_KEEPING_WAITS.windows(2).all(|w| w[0] < w[1]),
            "{FRAME_KEEPING_WAITS:?}"
        );
        let std_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std");
        for &(module, function) in FRAME_KEEPING_WAITS {
            let file = module.strip_prefix("std::").unwrap();
            let text = std::fs::read_to_string(std_dir.join(format!("{file}.cheby"))).unwrap();
            let natives = externals(&text);
            let lines: Vec<&str> = text.lines().collect();
            let at = lines
                .iter()
                .position(|l| {
                    l.starts_with(&format!("pub fn {function}("))
                        || l.starts_with(&format!("pub fn {function}<"))
                })
                .unwrap_or_else(|| panic!("{module}::{function} is not a pub fn"));
            let head = lines[at];
            assert!(
                !head.contains("fn("),
                "{module}::{function} takes a function value: {head}"
            );
            if natives.iter().any(|n| n == function) {
                assert!(
                    !head.ends_with('{'),
                    "{module}::{function} is a native wrapper with a body"
                );
                continue;
            }
            assert!(head.ends_with('{'), "{module}::{function} has no body");
            let body: Vec<&str> = lines[at + 1..]
                .iter()
                .take_while(|l| **l != "}")
                .copied()
                .collect();
            for line in &body {
                let mut rest = *line;
                while let Some((before, after)) = rest.split_once('(') {
                    let callee: String = before
                        .chars()
                        .rev()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect::<Vec<char>>()
                        .into_iter()
                        .rev()
                        .collect();
                    assert!(
                        natives.contains(&callee),
                        "{module}::{function} calls `{callee}`, not a native wrapper: {line}"
                    );
                    rest = after;
                }
            }
            assert!(keeps_frame(module, function));
        }
        assert!(!keeps_frame("std::selector", "select"));
        assert!(!keeps_frame("std::fiber", "scope"));
        assert!(!keeps_frame("app", "receive"));
    }
}
