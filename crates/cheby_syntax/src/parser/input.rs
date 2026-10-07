//! The parser's view of the raw tokens: trivia skipped, line breaks decided
//! by the newline policy (§2.8, D-231), and `>>`/`>=` split on demand.
//!
//! The lexer keeps every line break as a `NEWLINE` token. The token source
//! presents a `NEWLINE` to the parser only where it may end something; every
//! other one is skipped like whitespace. A line break is skipped when:
//!
//! - the innermost open delimiter is `(`, `[` or a type `<` (mode
//!   [`NlMode::Ignore`]);
//! - nothing has been read yet (start of file), or the last token read was a
//!   line break (consecutive line breaks merge) or `{`;
//! - the last token read continues the line: `,`, `=`, `=>`, `->`, `<-`, or a
//!   token the parser read as a binary operator;
//! - the next token after the line breaks, comments and blank lines is `|>`,
//!   `&&` or `||` (leading continuation), `}`, or the end of the file
//!   (D-426).
//!
//! Only the parser knows whether a token is a binary operator, so it says so
//! when it reads one ([`Input::advance`] with `binop`).

use std::cell::Cell;

use crate::SyntaxKind::{
    self, AMP2, COMMA, EOF, EQ, ERROR_TOKEN, FAT_ARROW, GE, GT, L_BRACE, LEFT_ARROW, NEWLINE,
    PIPE_GT, PIPE2, R_BRACE, SHR, THIN_ARROW,
};
use crate::TokenSet;
use crate::lexer::Lexed;

/// Whether line breaks are significant inside the innermost delimiter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum NlMode {
    /// Inside `(`, `[` or a type `<`: line breaks are whitespace.
    Ignore,
    /// Inside `{`, or at the top level: line breaks separate things.
    Significant,
}

/// Tokens after which a line always continues (§2.8 rule 2).
const TRAILING_CONTINUATION: TokenSet =
    TokenSet::new(&[COMMA, EQ, FAT_ARROW, THIN_ARROW, LEFT_ARROW]);

/// Tokens before which a line break is not significant: leading
/// continuation (§2.8 rule 3), a closing `}`, and the end of the file.
const SKIP_BEFORE: TokenSet = TokenSet::new(&[PIPE_GT, AMP2, PIPE2, R_BRACE, EOF]);

/// How many lookups the parser may make without consuming a token before
/// the input pretends to be at the end of the file. A correct parser needs
/// a few thousand at most (the depth limit times a small constant); the
/// limit only turns a parser bug into a truncated tree instead of a hang.
const FUEL: u32 = 1 << 20;

/// What the parser read last.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Last {
    /// Nothing yet.
    Start,
    Token {
        kind: SyntaxKind,
        binop: bool,
    },
}

/// The current token, as the parser sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Current {
    pub(super) kind: SyntaxKind,
    /// The raw token it is (part of).
    pub(super) raw: usize,
    /// Whether a line break (significant or not) comes between the last
    /// token read and this one, or nothing was read yet.
    pub(super) line_start: bool,
}

/// What [`Input::advance`] consumed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Consumed {
    /// Nothing: the input is at the end.
    Nothing,
    /// The whole raw token.
    Whole { raw: usize },
    /// Bytes `offset..offset + len` of the raw token, as `kind`.
    Part {
        raw: usize,
        kind: SyntaxKind,
        offset: u8,
        len: u8,
    },
}

pub(super) struct Input<'a> {
    lexed: &'a Lexed,
    text: &'a str,
    /// For each raw index `i`, the kind of the first raw token at or after
    /// `i` that is neither trivia nor a line break (`EOF` at the end).
    /// Computed once, so that deciding each line break of a long run of
    /// blank or comment lines takes constant time.
    next_significant: Vec<SyntaxKind>,
    /// The first raw token not (fully) consumed.
    pos: usize,
    /// How many bytes of the raw token at `pos` a split already consumed.
    split_off: u8,
    modes: Vec<NlMode>,
    last: Last,
    current: Cell<Option<Current>>,
    fuel: Cell<u32>,
    out_of_fuel: Cell<bool>,
}

impl<'a> Input<'a> {
    pub(super) fn new(lexed: &'a Lexed, text: &'a str) -> Self {
        let mut next_significant = vec![EOF; lexed.len()];
        let mut next = EOF;
        for (i, slot) in next_significant.iter_mut().enumerate().rev() {
            let kind = lexed.kind(i);
            if !(kind == NEWLINE || is_trivia(lexed, text, i)) {
                next = kind;
            }
            *slot = next;
        }
        Self {
            lexed,
            text,
            next_significant,
            pos: 0,
            split_off: 0,
            modes: Vec::new(),
            last: Last::Start,
            current: Cell::new(None),
            fuel: Cell::new(FUEL),
            out_of_fuel: Cell::new(false),
        }
    }

    /// Whether the fuel ran out, which means a parser bug.
    pub(super) fn out_of_fuel(&self) -> bool {
        self.out_of_fuel.get()
    }

    /// A position that changes whenever a token is consumed.
    pub(super) fn position(&self) -> (usize, u8) {
        (self.pos, self.split_off)
    }

    pub(super) fn current(&self) -> Current {
        let fuel = self.fuel.get();
        if fuel == 0 {
            self.out_of_fuel.set(true);
            return Current {
                kind: EOF,
                raw: self.lexed.len(),
                line_start: true,
            };
        }
        self.fuel.set(fuel.saturating_sub(1));
        if let Some(current) = self.current.get() {
            return current;
        }
        let current = self.compute();
        self.current.set(Some(current));
        current
    }

    fn invalidate(&mut self) {
        self.current.set(None);
    }

    /// Whether the raw token `i` is skipped like whitespace: trivia, and the
    /// error token of a block comment, which the lexer already reported
    /// (E0004).
    fn is_trivia(&self, i: usize) -> bool {
        is_trivia(self.lexed, self.text, i)
    }

    /// The kind of the first raw token at or after `i` that is neither
    /// trivia nor a line break.
    fn next_significant(&self, i: usize) -> SyntaxKind {
        self.next_significant.get(i).copied().unwrap_or(EOF)
    }

    fn top_mode(&self) -> NlMode {
        self.modes.last().copied().unwrap_or(NlMode::Significant)
    }

    /// Whether the line break at raw index `i` is skipped (module docs).
    fn newline_skipped(&self, i: usize) -> bool {
        if self.top_mode() == NlMode::Ignore {
            return true;
        }
        let continues = match self.last {
            Last::Start => true,
            Last::Token { kind, binop } => {
                binop || kind == NEWLINE || kind == L_BRACE || TRAILING_CONTINUATION.contains(kind)
            }
        };
        continues || SKIP_BEFORE.contains(self.next_significant(i.saturating_add(1)))
    }

    fn compute(&self) -> Current {
        if self.split_off > 0 {
            let kind = match self.lexed.kind(self.pos) {
                SHR => GT,
                GE => EQ,
                _ => ERROR_TOKEN,
            };
            return Current {
                kind,
                raw: self.pos,
                line_start: false,
            };
        }
        let mut line_start = matches!(self.last, Last::Start | Last::Token { kind: NEWLINE, .. });
        let mut i = self.pos;
        loop {
            let kind = self.lexed.kind(i);
            if kind == NEWLINE {
                line_start = true;
                if !self.newline_skipped(i) {
                    return Current {
                        kind,
                        raw: i,
                        line_start: false,
                    };
                }
            } else if kind == EOF || !self.is_trivia(i) {
                return Current {
                    kind,
                    raw: i,
                    line_start,
                };
            }
            i = i.saturating_add(1);
        }
    }

    /// The kind of the `n`-th token after the current one (`n >= 1`).
    ///
    /// Lookahead cannot know which later tokens the parser will read as
    /// binary operators or delimiters, so it decides line breaks with the
    /// current mode and the tokens that always continue a line. The parser
    /// looks ahead only over short, fixed shapes such as `name ::`.
    pub(super) fn nth(&self, n: usize) -> SyntaxKind {
        let current = self.current();
        if current.kind == EOF || n == 0 {
            return current.kind;
        }
        let ignore = self.top_mode() == NlMode::Ignore;
        let mut prev = current.kind;
        let mut seen = 0_usize;
        let mut i = current.raw.saturating_add(1);
        loop {
            let kind = self.lexed.kind(i);
            if kind == EOF {
                return EOF;
            }
            let skip = if kind == NEWLINE {
                ignore
                    || prev == NEWLINE
                    || prev == L_BRACE
                    || TRAILING_CONTINUATION.contains(prev)
                    || SKIP_BEFORE.contains(self.next_significant(i.saturating_add(1)))
            } else {
                self.is_trivia(i)
            };
            if !skip {
                seen = seen.saturating_add(1);
                if seen >= n {
                    return kind;
                }
                prev = kind;
            }
            i = i.saturating_add(1);
        }
    }

    /// Consumes the current token. `binop` says that the parser reads it as
    /// a binary operator, so a line break after it continues the line.
    pub(super) fn advance(&mut self, binop: bool) -> Consumed {
        let current = self.current();
        if current.kind == EOF {
            return Consumed::Nothing;
        }
        self.fuel.set(FUEL);
        self.last = Last::Token {
            kind: current.kind,
            binop,
        };
        let consumed = if self.split_off > 0 {
            let len = self
                .raw_len(current.raw)
                .saturating_sub(u32::from(self.split_off));
            Consumed::Part {
                raw: current.raw,
                kind: current.kind,
                offset: self.split_off,
                len: u8::try_from(len).unwrap_or(u8::MAX),
            }
        } else {
            Consumed::Whole { raw: current.raw }
        };
        self.pos = current.raw.saturating_add(1);
        self.split_off = 0;
        self.invalidate();
        consumed
    }

    /// Consumes the first byte of the current `>>` or `>=` as a `>` that
    /// closes type arguments (§2.6, D-430). The rest stays current, as `>`
    /// or `=`.
    pub(super) fn split_gt(&mut self) -> Consumed {
        let current = self.current();
        if self.split_off > 0 || !matches!(current.kind, SHR | GE) {
            return self.advance(false);
        }
        self.fuel.set(FUEL);
        self.last = Last::Token {
            kind: GT,
            binop: false,
        };
        self.pos = current.raw;
        self.split_off = 1;
        self.invalidate();
        Consumed::Part {
            raw: current.raw,
            kind: GT,
            offset: 0,
            len: 1,
        }
    }

    fn raw_len(&self, raw: usize) -> u32 {
        self.lexed.range(raw).len()
    }

    /// The byte range of the current token (only the rest, after a split).
    pub(super) fn current_range(&self) -> cheby_diag::TextRange {
        let current = self.current();
        let range = self.lexed.range(current.raw);
        let start = range
            .start()
            .saturating_add(u32::from(self.split_off))
            .min(range.end());
        cheby_diag::TextRange::new(start, range.end())
    }

    /// The text of the current token.
    pub(super) fn current_text(&self) -> &'a str {
        self.current_range().slice(self.text).unwrap_or_default()
    }

    pub(super) fn push_mode(&mut self, mode: NlMode) {
        self.modes.push(mode);
        self.invalidate();
    }

    /// The depth of the mode stack, to restore with [`Self::truncate_modes`].
    pub(super) fn mode_depth(&self) -> usize {
        self.modes.len()
    }

    pub(super) fn truncate_modes(&mut self, depth: usize) {
        if self.modes.len() > depth {
            self.modes.truncate(depth);
            self.invalidate();
        }
    }
}

/// Whether the raw token `i` is skipped like whitespace: trivia, and the
/// error token of a block comment, which the lexer already reported (E0004).
fn is_trivia(lexed: &Lexed, text: &str, i: usize) -> bool {
    let kind = lexed.kind(i);
    kind.is_trivia() || (kind == ERROR_TOKEN && lexed.text(text, i).starts_with("/*"))
}
