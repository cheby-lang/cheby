//! The std waits whose tail calls keep their caller's frame (D-861).
//!
//! A tail call from a function outside package `std` to one of these
//! functions is lowered as a call followed by a return, so the caller's
//! frame stays on the stack while the fiber waits and a deadlock report can
//! name the caller's call (D-856). Each never calls a function value, so the
//! change adds at most one frame per waiting fiber and no loop can grow the
//! stack through it. `selector::select` is not here: it calls the chosen
//! arm's function in tail position, and select-driven loops rely on that.
//! D-1059 extends the rule to every public std wait that calls no function
//! value, such as `channel::to_list` and `channel::send_all` (D-1100). A
//! test reads each module's source and checks that the function exists and
//! calls no function value.

/// `(module, function)`, sorted by module and function.
pub const FRAME_KEEPING_WAITS: &[(&str, &str)] = &[
    ("std::channel", "receive"),
    ("std::channel", "send"),
    ("std::channel", "send_all"),
    ("std::channel", "to_list"),
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

    /// The line at which `lines` declares the top-level function
    /// `function`, `pub` or not.
    fn find_fn(lines: &[&str], function: &str) -> Option<usize> {
        lines.iter().position(|l| {
            let rest = l.strip_prefix("pub ").unwrap_or(l);
            rest.starts_with(&format!("fn {function}("))
                || rest.starts_with(&format!("fn {function}<"))
        })
    }

    /// The name before each `(` of `line` outside a comment: a callee, a
    /// constructor, or the empty name of a tuple or a grouping.
    fn callees(line: &str) -> Vec<String> {
        let code = line.split_once("//").map_or(line, |(code, _)| code);
        let mut out = Vec::new();
        let mut rest = code;
        while let Some((before, after)) = rest.split_once('(') {
            let callee: Vec<char> = before
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            out.push(callee.into_iter().rev().collect());
            rest = after;
        }
        out
    }

    /// Checks that the function declared at `lines[at]` takes no function
    /// value and calls only `natives`, constructors, itself and other
    /// functions of its module that pass the same check, which `checked`
    /// collects. A native wrapper itself has no body.
    fn check_calls(
        module: &str,
        lines: &[&str],
        natives: &[String],
        at: usize,
        checked: &mut Vec<String>,
    ) {
        let head = lines[at];
        let name: String = head
            .trim_start_matches("pub ")
            .trim_start_matches("fn ")
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if checked.contains(&name) {
            return;
        }
        checked.push(name.clone());
        assert!(
            !head.contains("fn("),
            "{module}::{name} takes a function value: {head}"
        );
        if natives.contains(&name) {
            assert!(
                !head.ends_with('{'),
                "{module}::{name} is a native wrapper with a body"
            );
            return;
        }
        assert!(head.ends_with('{'), "{module}::{name} has no body");
        let body = lines[at..].iter().skip(1).take_while(|l| **l != "}");
        for line in body {
            for callee in callees(line) {
                let constructor = callee.starts_with(|c: char| c.is_ascii_uppercase());
                if callee.is_empty() || constructor || natives.contains(&callee) {
                    continue;
                }
                let helper = find_fn(lines, &callee).unwrap_or_else(|| {
                    panic!(
                        "{module}::{name} calls `{callee}`, not a native wrapper or a \
                         function of its module: {line}"
                    )
                });
                check_calls(module, lines, natives, helper, checked);
            }
        }
    }

    /// The table is sorted without duplicates, and each entry is a `pub`
    /// function of its std module that takes no function value and calls
    /// only the module's `@external(native, …)` functions, constructors,
    /// and functions of the module that pass the same check, such as
    /// itself: a native wrapper itself (no body), or a body whose every
    /// call is one of those.
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
            check_calls(module, &lines, &natives, at, &mut Vec::new());
            assert!(keeps_frame(module, function));
        }
        // A wait that calls a function value fails the check.
        let lines = [
            "pub fn drain(r: Receiver<Int>) -> Int {",
            "  helper(r, fn(n) { n })",
            "}",
            "fn helper(r: Receiver<Int>, f: fn(Int) -> Int) -> Int {",
            "  f(1)",
            "}",
        ];
        let caught = std::panic::catch_unwind(|| {
            check_calls("std::test", &lines, &[], 0, &mut Vec::new());
        });
        assert!(caught.is_err());
        assert!(!keeps_frame("std::selector", "select"));
        assert!(!keeps_frame("std::fiber", "scope"));
        assert!(!keeps_frame("app", "receive"));
    }
}
