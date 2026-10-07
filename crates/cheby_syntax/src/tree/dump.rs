//! The indented text form of a tree, used by snapshot tests.
//!
//! One element per line, in preorder, indented by two spaces per depth:
//! `KIND@start..end` for a node, followed by the text in Rust debug quoting
//! for a token.

use std::fmt::Write;

use super::{SyntaxTree, id};

pub(super) fn dump(tree: &SyntaxTree) -> String {
    let mut out = String::new();
    // Subtree ends of the open ancestors of the current element.
    let mut open: Vec<u32> = Vec::new();
    for (i, e) in tree.elements.iter().enumerate() {
        let i = id(i);
        while open.last().is_some_and(|&end| end <= i) {
            open.pop();
        }
        for _ in 0..open.len() {
            out.push_str("  ");
        }
        _ = write!(out, "{}@{}..{}", e.kind.name(), e.start, e.end);
        if e.kind.is_node() {
            open.push(e.subtree_end);
        } else {
            let text = cheby_diag::TextRange::new(e.start, e.end)
                .slice(&tree.text)
                .unwrap_or_default();
            _ = write!(out, " {text:?}");
        }
        out.push('\n');
    }
    out
}
