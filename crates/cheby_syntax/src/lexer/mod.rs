//! The lexer: source text to a lossless stream of raw tokens (ch. 2).
//!
//! [`lex`] never fails. Every byte of the input belongs to exactly one
//! token, so the concatenated token texts are the input. Characters that
//! start no token become [`SyntaxKind::ERROR_TOKEN`]s with a diagnostic.
//!
//! The lexer reports only grammar errors, the ones `tree-sitter-cheby` also
//! rejects (class G in the phase-A design). Lexical rules that tree-sitter
//! does not enforce, such as number shapes and escape validity, are
//! checked later by [`checks`], called from validation. Naming conventions
//! and reserved words are checked by name resolution where a name is bound
//! ([`crate::names`]).
//!
//! The lexer does not merge or drop line breaks: each one is a
//! [`SyntaxKind::NEWLINE`] token, and the parser decides which ones end a
//! statement (§2.8, D-231).

pub(crate) mod checks;
pub mod literal;
mod number;
mod scan;

use cheby_diag::{Diagnostic, FileId, TextRange, TextSize};

use crate::SyntaxKind;

/// A token as the lexer produced it: its kind and start offset. Its end is
/// the start of the next token, or the end of the text for the last one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawToken {
    pub kind: SyntaxKind,
    pub start: TextSize,
}

/// The tokens of one file and the lexical errors found in it.
///
/// Token ranges are contiguous, start at 0, end at the length of the text,
/// and lie on character boundaries.
#[derive(Clone, Debug, Default)]
pub struct Lexed {
    tokens: Vec<RawToken>,
    end: TextSize,
    errors: Vec<Diagnostic>,
}

impl Lexed {
    /// The number of tokens.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether there are no tokens, which happens only for empty text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// All tokens, in order.
    #[must_use]
    pub fn tokens(&self) -> &[RawToken] {
        &self.tokens
    }

    /// The kind of token `i`, or [`SyntaxKind::EOF`] past the end.
    #[must_use]
    pub fn kind(&self, i: usize) -> SyntaxKind {
        self.tokens.get(i).map_or(SyntaxKind::EOF, |t| t.kind)
    }

    /// The range of token `i`. Past the end, it is the empty range at the
    /// end of the text.
    #[must_use]
    pub fn range(&self, i: usize) -> TextRange {
        let Some(token) = self.tokens.get(i) else {
            return TextRange::empty(self.end);
        };
        let end = i
            .checked_add(1)
            .and_then(|next| self.tokens.get(next))
            .map_or(self.end, |next| next.start);
        TextRange::new(token.start, end)
    }

    /// The text of token `i` in `src`, the text that was lexed. Empty past
    /// the end or for another text.
    #[must_use]
    pub fn text<'a>(&self, src: &'a str, i: usize) -> &'a str {
        self.range(i).slice(src).unwrap_or_default()
    }

    /// The length of the lexed text.
    #[must_use]
    pub const fn text_len(&self) -> TextSize {
        self.end
    }

    /// The lexical grammar errors, in source order.
    #[must_use]
    pub fn errors(&self) -> &[Diagnostic] {
        &self.errors
    }

    /// Takes the errors out, leaving none.
    pub fn take_errors(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.errors)
    }
}

/// Splits `text` into tokens. `file` is used for the spans of diagnostics.
///
/// Never fails. A text longer than `u32::MAX` bytes cannot come from a
/// [`cheby_diag::SourceMap`]; for such a text, offsets saturate.
#[must_use]
pub fn lex(file: FileId, text: &str) -> Lexed {
    scan::Scanner::new(file, text).run()
}

#[cfg(test)]
mod tests;
