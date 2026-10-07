//! The lexer never panics, and its tokens cover the input contiguously,
//! on character boundaries (§2, A2).

#![no_main]

use cheby_diag::FileId;
use cheby_syntax::lexer::lex;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let lexed = lex(FileId::new(0), text);
    let mut end = 0;
    for i in 0..lexed.len() {
        let range = lexed.range(i);
        assert_eq!(range.start(), end, "token {i} is not contiguous");
        assert!(range.slice(text).is_some(), "token {i} splits a character");
        end = range.end();
    }
    assert_eq!(usize::try_from(end).ok(), Some(text.len()));
});
