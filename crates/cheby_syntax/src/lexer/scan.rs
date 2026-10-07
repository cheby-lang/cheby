//! The lexer's main loop: a hand-written scanner over bytes with three
//! modes (normal, string, interpolation).
//!
//! Token rules follow §2.2-§2.6 and mirror `tree-sitter-cheby` where the
//! spec leaves room, so that what tree-sitter accepts lexes without a
//! grammar error here.

#![expect(
    clippy::arithmetic_side_effects,
    reason = "positions are bounded by `text.len()`, and each step adds at most the length of \
              the rest of the text, so no addition overflows"
)]

use cheby_diag::{Diagnostic, FileId, Span, TextRange, TextSize, codes};

use super::number::{NumberError, scan_number};
use super::{Lexed, RawToken};
use crate::SyntaxKind;

/// What the scanner is inside.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Normal,
    /// Inside a string literal that starts at byte `start`.
    Str {
        start: usize,
    },
    /// Inside the braces of an interpolation in the string at `start`.
    Interp {
        start: usize,
    },
}

pub(super) struct Scanner<'a> {
    file: FileId,
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    mode: Mode,
    tokens: Vec<RawToken>,
    errors: Vec<Diagnostic>,
}

/// A byte offset as a [`TextSize`]. Texts from a `SourceMap` fit.
fn offset(pos: usize) -> TextSize {
    TextSize::try_from(pos).unwrap_or(TextSize::MAX)
}

const fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

impl<'a> Scanner<'a> {
    pub(super) fn new(file: FileId, text: &'a str) -> Self {
        Self {
            file,
            text,
            bytes: text.as_bytes(),
            pos: 0,
            mode: Mode::Normal,
            // Source averages a little over three bytes per token.
            tokens: Vec::with_capacity(text.len() / 3 + 1),
            errors: Vec::new(),
        }
    }

    pub(super) fn run(mut self) -> Lexed {
        while self.pos < self.bytes.len() {
            match self.mode {
                Mode::Normal => self.normal(false),
                Mode::Str { start } => self.string(start),
                Mode::Interp { start } => self.interp(start),
            }
        }
        if let Mode::Str { start } | Mode::Interp { start } = self.mode {
            self.error(
                Diagnostic::new(codes::UNTERMINATED_STRING, "unterminated string")
                    .with_primary(self.span(start, start + 1), "this string is never closed")
                    .with_help("add a closing `\"`"),
            );
        }
        Lexed {
            tokens: self.tokens,
            end: offset(self.bytes.len()),
            errors: self.errors,
        }
    }

    fn at(&self, i: usize) -> Option<u8> {
        self.bytes.get(i).copied()
    }

    fn peek(&self, ahead: usize) -> Option<u8> {
        self.at(self.pos + ahead)
    }

    fn char_at(&self, i: usize) -> Option<char> {
        self.text.get(i..).and_then(|rest| rest.chars().next())
    }

    /// The length of the character at `i`, at least 1.
    fn char_len(&self, i: usize) -> usize {
        self.char_at(i).map_or(1, char::len_utf8)
    }

    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, TextRange::new(offset(start), offset(end)))
    }

    fn error(&mut self, diag: Diagnostic) {
        self.errors.push(diag);
    }

    /// Pushes a token of `kind` from the current position to `end`. A token
    /// is never empty: an `end` at or before the position is moved to the
    /// end of the current character, so the scanner always advances.
    fn push(&mut self, kind: SyntaxKind, end: usize) {
        let end = if end > self.pos {
            end.min(self.bytes.len())
        } else {
            self.pos + self.char_len(self.pos)
        };
        self.tokens.push(RawToken {
            kind,
            start: offset(self.pos),
        });
        self.pos = end;
    }

    /// The end of the run of bytes matching `pred` that starts at `i`.
    fn run_while(&self, mut i: usize, pred: impl Fn(u8) -> bool) -> usize {
        while self.at(i).is_some_and(&pred) {
            i += 1;
        }
        i
    }

    // ---- normal mode ----------------------------------------------------

    /// Lexes one token in normal mode. Inside an interpolation, comments and
    /// raw strings are not recognized (`/` is `SLASH`, `r` an identifier).
    fn normal(&mut self, in_interp: bool) {
        let pos = self.pos;
        let Some(b) = self.at(pos) else {
            return;
        };
        match b {
            b' ' | b'\t' => {
                let end = self.run_while(pos, |b| b == b' ' || b == b'\t');
                self.push(SyntaxKind::WHITESPACE, end);
            }
            b'\n' => self.push(SyntaxKind::NEWLINE, pos + 1),
            b'\r' if self.peek(1) == Some(b'\n') => self.push(SyntaxKind::NEWLINE, pos + 2),
            // A lone CR is whitespace here and E0002 in validation (§2.1),
            // as tree-sitter treats it as whitespace.
            b'\r' => self.push(SyntaxKind::WHITESPACE, pos + 1),
            b'/' if !in_interp && self.peek(1) == Some(b'/') => self.comment(),
            b'/' if !in_interp && self.peek(1) == Some(b'*') => self.block_comment(),
            b'"' => {
                self.push(SyntaxKind::STRING_START, pos + 1);
                self.mode = Mode::Str { start: pos };
            }
            b'r' if !in_interp && self.raw_string_hashes().is_some() => self.raw_string(),
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.ident(),
            b'0'..=b'9' => self.number(),
            0x80.. => self.non_ascii(),
            _ => self.punct(b),
        }
    }

    /// `//`, `///` and `//!` comments, up to the end of the line (§2.2,
    /// D-428): `//!…` is a module doc, `///` followed by the end of the line
    /// or a character other than `/` is a doc comment, and everything else,
    /// including `////…`, is an ordinary comment.
    fn comment(&mut self) {
        let pos = self.pos;
        let kind = match (self.peek(2), self.peek(3)) {
            (Some(b'!'), _) => SyntaxKind::MODULE_DOC,
            (Some(b'/'), Some(b'/')) => SyntaxKind::COMMENT,
            (Some(b'/'), _) => SyntaxKind::DOC_COMMENT,
            _ => SyntaxKind::COMMENT,
        };
        let newline = self.run_while(pos, |b| b != b'\n');
        // The `\r` of a CRLF belongs to the line break, a lone CR to the
        // comment.
        let end = if newline < self.bytes.len() && self.at(newline - 1) == Some(b'\r') {
            newline - 1
        } else {
            newline
        };
        self.push(kind, end);
    }

    /// `/* … */` is not Cheby (D-078). It becomes one error token, through
    /// the next `*/` or to the end of the file. Block comments do not nest.
    fn block_comment(&mut self) {
        let pos = self.pos;
        let end = self
            .bytes
            .get(pos + 2..)
            .and_then(|rest| rest.windows(2).position(|w| w == b"*/"))
            .map_or(self.bytes.len(), |i| pos + 2 + i + 2);
        self.error(
            Diagnostic::new(codes::BLOCK_COMMENT, "block comments are not supported")
                .with_primary(self.span(pos, pos + 2), "block comment starts here")
                .with_help("start each line of the comment with `//` (D-078)"),
        );
        self.push(SyntaxKind::ERROR_TOKEN, end);
    }

    /// If a raw string starts at the current `r`, the number of `#` in its
    /// opening delimiter.
    fn raw_string_hashes(&self) -> Option<usize> {
        let quote = self.run_while(self.pos + 1, |b| b == b'#');
        (self.at(quote) == Some(b'"')).then(|| quote - self.pos - 1)
    }

    /// `r"…"`, `r#"…"#` and so on (§2.5.3, D-130). The string ends at the
    /// first `"` followed by as many `#` as the opening delimiter has.
    fn raw_string(&mut self) {
        let pos = self.pos;
        let hashes = self.raw_string_hashes().unwrap_or(0);
        let body = pos + 1 + hashes + 1;
        let mut i = body;
        let end = loop {
            let Some(quote) = self
                .bytes
                .get(i..)
                .and_then(|rest| rest.iter().position(|&b| b == b'"'))
                .map(|q| i + q)
            else {
                break None;
            };
            let closer = quote + 1 + hashes;
            if self
                .bytes
                .get(quote + 1..closer)
                .is_some_and(|h| h.iter().all(|&b| b == b'#'))
            {
                break Some(closer);
            }
            i = quote + 1;
        };
        if end.is_none() {
            let closer = format!("\"{}", "#".repeat(hashes));
            self.error(
                Diagnostic::new(codes::UNTERMINATED_STRING, "unterminated raw string")
                    .with_primary(self.span(pos, body), "this raw string is never closed")
                    .with_help(format!("end the raw string with `{closer}`")),
            );
        }
        self.push(SyntaxKind::RAW_STRING, end.unwrap_or(self.bytes.len()));
    }

    /// Identifiers and keywords (§2.3, §2.4). The token rules are
    /// permissive like tree-sitter's: `[a-z_][A-Za-z0-9_]*` and
    /// `[A-Z][A-Za-z0-9_]*`. Spelling is checked by validation (E0006,
    /// E0007). Non-ASCII letters join the identifier and are a grammar
    /// error (E0005, D-127).
    fn ident(&mut self) {
        let pos = self.pos;
        let mut end = self.run_while(pos, is_word);
        let mut non_ascii = None;
        while let Some(c) = self.char_at(end) {
            if c.is_ascii() || !(c.is_alphanumeric()) {
                break;
            }
            non_ascii.get_or_insert((end, c));
            end = self.run_while(end + c.len_utf8(), is_word);
        }
        let text = self.text.get(pos..end).unwrap_or_default();
        let first_upper = text.chars().next().is_some_and(char::is_uppercase);
        let kind = if let Some((at, c)) = non_ascii {
            self.non_ascii_error(at, c);
            if first_upper {
                SyntaxKind::UPPER_IDENT
            } else {
                SyntaxKind::LOWER_IDENT
            }
        } else if text == "_" {
            SyntaxKind::UNDERSCORE
        } else if let Some(keyword) = SyntaxKind::from_keyword(text) {
            keyword
        } else if first_upper {
            SyntaxKind::UPPER_IDENT
        } else {
            SyntaxKind::LOWER_IDENT
        };
        self.push(kind, end);
    }

    fn non_ascii_error(&mut self, at: usize, c: char) {
        self.error(
            Diagnostic::new(
                codes::NON_ASCII_IDENTIFIER,
                format!("non-ASCII character `{c}` in an identifier"),
            )
            .with_primary(self.span(at, at + c.len_utf8()), "not an ASCII letter")
            .with_help("identifiers use only ASCII letters, digits and `_` (D-127)"),
        );
    }

    /// A character of 128 or above: a byte-order mark at the start of the
    /// file, a non-ASCII letter starting an identifier, or an error.
    fn non_ascii(&mut self) {
        let pos = self.pos;
        let Some(c) = self.char_at(pos) else {
            // Unreachable for a `&str`; advance safely anyway.
            self.push(SyntaxKind::ERROR_TOKEN, pos + 1);
            return;
        };
        if pos == 0 && c == '\u{feff}' {
            // The BOM is ignored (§2.1).
            self.push(SyntaxKind::WHITESPACE, pos + c.len_utf8());
        } else if c.is_alphabetic() {
            self.ident();
        } else {
            self.unexpected(c);
        }
    }

    /// Numbers (§2.5.1, §2.5.2), with suffixes and wrong digits absorbed
    /// into the token as grammar errors (E0008, E0009).
    fn number(&mut self) {
        let pos = self.pos;
        let scan = scan_number(self.bytes.get(pos..).unwrap_or_default());
        let end = pos + scan.len;
        let text = self.text.get(pos..end).unwrap_or_default();
        match scan.error {
            None => {}
            Some(NumberError::NoDigits) => {
                let prefix = scan.base.prefix();
                self.error(
                    Diagnostic::new(
                        codes::MALFORMED_NUMBER,
                        format!(
                            "`{prefix}` is not followed by any {} digit",
                            scan.base.name()
                        ),
                    )
                    .with_primary(
                        self.span(pos, end),
                        format!("expected digits after `{prefix}`"),
                    ),
                );
            }
            Some(NumberError::DigitOutOfBase { at }) => {
                let digits = text.get(at..).unwrap_or_default();
                self.error(
                    Diagnostic::new(
                        codes::MALFORMED_NUMBER,
                        format!(
                            "invalid digits `{digits}` in a {} literal",
                            scan.base.name()
                        ),
                    )
                    .with_primary(
                        self.span(pos + at, end),
                        format!("not {} digits", scan.base.name()),
                    ),
                );
            }
            Some(NumberError::Suffix { at }) => {
                let suffix = text.get(at..).unwrap_or_default();
                let example = suffix_example(text.get(..at).unwrap_or_default(), suffix);
                self.error(
                    Diagnostic::new(
                        codes::NUMBER_SUFFIX,
                        format!("unexpected suffix `{suffix}` after a number"),
                    )
                    .with_primary(self.span(pos + at, end), "numbers have no suffixes")
                    .with_help(format!(
                        "a literal's type comes from context; annotate the binding instead, \
                         such as `{example}` (D-084)"
                    )),
                );
            }
        }
        self.push(scan.kind, end);
    }

    /// Operators and punctuation, longest match (§2.6).
    fn punct(&mut self, b: u8) {
        use SyntaxKind as K;
        let next = self.peek(1);
        let (kind, len) = match (b, next) {
            (b'|', Some(b'>')) => (K::PIPE_GT, 2),
            (b'|', Some(b'|')) => (K::PIPE2, 2),
            (b'|', _) => (K::PIPE, 1),
            (b'&', Some(b'&')) => (K::AMP2, 2),
            (b'&', _) => (K::AMP, 1),
            (b'=', Some(b'=')) => (K::EQ2, 2),
            (b'=', Some(b'>')) => (K::FAT_ARROW, 2),
            (b'=', _) => (K::EQ, 1),
            (b'!', Some(b'=')) => (K::NEQ, 2),
            (b'!', _) => (K::BANG, 1),
            (b'<', Some(b'=')) => (K::LE, 2),
            (b'<', Some(b'<')) => (K::SHL, 2),
            (b'<', Some(b'-')) => (K::LEFT_ARROW, 2),
            (b'<', _) => (K::LT, 1),
            (b'>', Some(b'=')) => (K::GE, 2),
            (b'>', Some(b'>')) => (K::SHR, 2),
            (b'>', _) => (K::GT, 1),
            (b'-', Some(b'>')) => (K::THIN_ARROW, 2),
            (b'-', _) => (K::MINUS, 1),
            (b':', Some(b':')) => (K::COLON2, 2),
            (b':', Some(b'?')) => (K::COLON_QUESTION, 2),
            (b':', _) => (K::COLON, 1),
            (b'.', Some(b'.')) => (K::DOT2, 2),
            (b'.', _) => (K::DOT, 1),
            (b'@', Some(b'!')) => (K::AT_BANG, 2),
            (b'@', _) => (K::AT, 1),
            (b'+', _) => (K::PLUS, 1),
            (b'*', _) => (K::STAR, 1),
            (b'/', _) => (K::SLASH, 1),
            (b'%', _) => (K::PERCENT, 1),
            (b'^', _) => (K::CARET, 1),
            (b',', _) => (K::COMMA, 1),
            (b'(', _) => (K::L_PAREN, 1),
            (b')', _) => (K::R_PAREN, 1),
            (b'[', _) => (K::L_BRACK, 1),
            (b']', _) => (K::R_BRACK, 1),
            (b'{', _) => (K::L_BRACE, 1),
            (b'}', _) => (K::R_BRACE, 1),
            _ => {
                self.unexpected(char::from(b));
                return;
            }
        };
        self.push(kind, self.pos + len);
    }

    /// A character that starts no token (E0003): one error token per
    /// character.
    fn unexpected(&mut self, c: char) {
        let pos = self.pos;
        let end = pos + c.len_utf8();
        let shown: String = if c.is_control() || c.is_whitespace() {
            c.escape_unicode().collect()
        } else {
            c.to_string()
        };
        let mut diag = Diagnostic::new(
            codes::UNEXPECTED_CHARACTER,
            format!("unexpected character `{shown}`"),
        )
        .with_primary(self.span(pos, end), "");
        diag = match c {
            ';' => diag.with_help("statements are separated by line breaks, not `;`"),
            '\'' => diag.with_help("strings use double quotes, such as \"text\""),
            _ => diag,
        };
        self.error(diag);
        self.push(SyntaxKind::ERROR_TOKEN, end);
    }

    // ---- string mode ----------------------------------------------------

    /// One token inside a string literal (§2.5.3).
    fn string(&mut self, start: usize) {
        let pos = self.pos;
        match self.at(pos) {
            Some(b'"') => {
                self.push(SyntaxKind::STRING_END, pos + 1);
                self.mode = Mode::Normal;
            }
            Some(b'\\') => self.escape(),
            Some(b'{') => {
                self.push(SyntaxKind::INTERP_START, pos + 1);
                self.mode = Mode::Interp { start };
            }
            _ => {
                let end = self.run_while(pos, |b| !matches!(b, b'"' | b'\\' | b'{'));
                self.push(SyntaxKind::STRING_TEXT, end);
            }
        }
    }

    /// An escape: `\u{` + hex digits + `}` when the `}` follows the digits
    /// directly, as tree-sitter's `\u\{[0-9a-fA-F]*\}`, otherwise `\` and
    /// exactly one character. Validity (D-114) is checked by validation
    /// (E0011), because tree-sitter accepts any escape.
    fn escape(&mut self) {
        let pos = self.pos;
        let end = match self.peek(1) {
            None => pos + 1,
            Some(b'u') if self.peek(2) == Some(b'{') => {
                let digits_end = self.run_while(pos + 3, |b| b.is_ascii_hexdigit());
                if self.at(digits_end) == Some(b'}') {
                    digits_end + 1
                } else {
                    pos + 2
                }
            }
            Some(_) => pos + 1 + self.char_len(pos + 1),
        };
        self.push(SyntaxKind::STRING_ESCAPE, end);
    }

    // ---- interpolation mode ---------------------------------------------

    /// One token inside `{…}` of a string (§2.5.4). `}` ends the
    /// interpolation. A `"` or a line break ends it without being consumed,
    /// and is then lexed in string mode. Everything else is a normal-mode
    /// token; the parser checks the shape (E0012).
    fn interp(&mut self, start: usize) {
        let pos = self.pos;
        match (self.at(pos), self.peek(1)) {
            (Some(b'}'), _) => {
                self.push(SyntaxKind::INTERP_END, pos + 1);
                self.mode = Mode::Str { start };
            }
            (Some(b'"' | b'\n'), _) | (Some(b'\r'), Some(b'\n')) => {
                self.mode = Mode::Str { start };
            }
            _ => self.normal(true),
        }
    }
}

/// The example binding of the E0009 help for `digits` followed by
/// `suffix`: `1u8` gives `let x: U8 = 1`. A suffix that names no sized
/// type (D-105) gives `let x: U8 = 1`.
fn suffix_example(digits: &str, suffix: &str) -> String {
    const SIZED: [&str; 9] = ["I8", "I16", "I32", "I64", "U8", "U16", "U32", "U64", "F32"];
    let named = SIZED
        .iter()
        .find(|ty| ty.eq_ignore_ascii_case(suffix) && !digits.is_empty());
    match named {
        Some(ty) => format!("let x: {ty} = {digits}"),
        None => "let x: U8 = 1".to_owned(),
    }
}
