//! Parsing and validation never panic, and the tree prints back exactly
//! the input (D-280, A3, A8).

#![no_main]

use std::sync::Arc;

use cheby_diag::FileId;
use cheby_syntax::{parse, validate};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let parse = parse(FileId::new(0), Arc::from(text));
    let tree = parse.tree();
    let printed: String = tree.root().tokens().map(|t| t.text()).collect();
    assert_eq!(printed, text);
    let root = tree.root().text_range();
    for node in tree.root().descendants() {
        assert!(root.contains_range(node.text_range()));
    }
    let _ = validate(tree);
});
