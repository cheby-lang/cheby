//! The parser: raw tokens to events (A4-A8, D-272, D-280).
//!
//! A hand-written recursive-descent parser over Appendix A. It reads tokens
//! through [`input::Input`], which applies the newline policy of §2.8
//! (D-231), and emits [`Event`]s that [`crate::tree::build`] turns into the
//! lossless tree. Markers work as in rust-analyzer: [`Parser::start`] opens a
//! node, [`Marker::complete`] closes it with its kind, and
//! [`CompletedMarker::precede`] wraps a finished node in a new one.
//!
//! The parser never fails and never panics. It always produces a tree that
//! covers the whole input, reports grammar errors (class G) as diagnostics
//! with codes, and recovers by skipping into `ERROR` nodes (A8). Every loop
//! consumes a token or exits, and a fuel counter in the input turns a loop
//! that does not into a truncated parse instead of a hang. Nesting is
//! limited to [`MAX_DEPTH`] levels (E0016).
//!
//! Where the spec rejects a shape that `tree-sitter-cheby` accepts, the
//! parser accepts and validation reports (D-424). Line breaks (§2.8),
//! lexical errors (§2.1, §2.5) and reserved keywords (§2.4) stay grammar
//! errors even where `tree-sitter-cheby` accepts them; those cases are
//! grammar mismatches.

mod grammar;
mod input;
mod recovery;

use std::num::NonZeroU32;

use cheby_diag::codes;
use cheby_diag::{Diagnostic, FileId, Span, TextRange, TextSize};

use self::input::{Consumed, Input, NlMode};
use crate::SyntaxKind::{
    self, EOF, ERROR, ERROR_TOKEN, FN_KW, GE, GT, L_BRACE, L_BRACK, L_PAREN, LOWER_IDENT, LT,
    NEWLINE, R_BRACE, R_BRACK, R_PAREN, SHR, WHITESPACE,
};
use crate::TokenSet;
use crate::event::Event;
use crate::lexer::Lexed;

/// The deepest nesting of expressions, patterns, types and blocks (E0016).
pub(crate) const MAX_DEPTH: u32 = 256;

/// Parses the tokens of `text` into events and grammar errors, in source
/// order.
pub(crate) fn parse(file: FileId, lexed: &Lexed, text: &str) -> (Vec<Event>, Vec<Diagnostic>) {
    let (events, errors, _) = parse_with_fuel(file, lexed, text);
    (events, errors)
}

/// [`parse`], also saying whether the fuel ran out (a parser bug; tests
/// check that it never does).
fn parse_with_fuel(file: FileId, lexed: &Lexed, text: &str) -> (Vec<Event>, Vec<Diagnostic>, bool) {
    let mut p = Parser::new(file, lexed, text);
    grammar::source_file(&mut p);
    let out_of_fuel = p.input.out_of_fuel();
    let mut errors = p.errors;
    errors.sort_by_key(|d| d.primary_span().map(|s| s.range.start()));
    (p.events, errors, out_of_fuel)
}

pub(crate) struct Parser<'a> {
    file: FileId,
    input: Input<'a>,
    lexed: &'a Lexed,
    events: Vec<Event>,
    errors: Vec<Diagnostic>,
    /// The raw index of the last error, to report one error per token.
    last_error_raw: Option<usize>,
    /// The end of the last token read that is not a line break.
    prev_end: TextSize,
    depth: u32,
    too_deep_reported: bool,
    /// Whether the file ends inside an unterminated string or raw string,
    /// which the lexer reported (E0010). Errors at the end of the file are
    /// then only consequences, and are not reported.
    ends_in_string: bool,
    /// The kinds of the delimiters the grammar opened and has not closed
    /// yet, innermost last.
    open_delims: Vec<SyntaxKind>,
    /// What matching the raw `{` and `}` tokens says about the file.
    braces: Braces,
    /// Set when the depth limit stopped parsing (E0016). The errors that
    /// follow until the next top-level item are consequences, and are not
    /// reported.
    unwinding: bool,
}

/// The `{` and `}` tokens of the file matched like brackets, ignoring the
/// grammar: each `}` closes the nearest open `{`. Recovery uses it to tell
/// whether a body can still be closed later in the file.
#[derive(Default)]
struct Braces {
    /// Raw indices of the `{` that no `}` closes, ascending.
    unmatched: Vec<usize>,
    /// Raw indices of the `{` whose `}` starts a line indented differently
    /// from the line of the `{`, ascending. When a body is never closed,
    /// one of these is likely the brace whose `}` is missing.
    misindented: Vec<usize>,
}

impl Braces {
    fn new(lexed: &Lexed, text: &str) -> Self {
        let mut braces = Self::default();
        // Open braces with the indentation of their line.
        let mut stack: Vec<(usize, usize)> = Vec::new();
        let mut indent = 0_usize;
        let mut line_start = true;
        for raw in 0..lexed.len() {
            let kind = lexed.kind(raw);
            match kind {
                NEWLINE => {
                    indent = 0;
                    line_start = true;
                    continue;
                }
                WHITESPACE if line_start => {
                    indent = lexed.text(text, raw).len();
                    continue;
                }
                L_BRACE => stack.push((raw, indent)),
                R_BRACE => {
                    if let Some((open, open_indent)) = stack.pop()
                        && line_start
                        && open_indent != indent
                    {
                        braces.misindented.push(open);
                    }
                }
                _ => {}
            }
            line_start = false;
        }
        braces.misindented.sort_unstable();
        braces.unmatched = stack.into_iter().map(|(raw, _)| raw).collect();
        braces
    }
}

/// An open node. Complete it with [`Marker::complete`] or drop it with
/// [`Marker::abandon`]; a marker that is simply dropped leaves no node.
#[must_use]
pub(crate) struct Marker {
    pos: usize,
}

/// A finished node, which can still be wrapped with [`Self::precede`].
#[derive(Clone, Copy)]
pub(crate) struct CompletedMarker {
    pos: usize,
}

/// An opened delimiter, closed with [`Parser::close_delim`].
#[must_use]
#[derive(Clone, Copy)]
pub(crate) struct Delim {
    mode_depth: usize,
    /// The depth of [`Parser::open_delims`] before this delimiter.
    stack_depth: usize,
    open: SyntaxKind,
    /// The raw index of the opening token.
    raw: usize,
    range: TextRange,
}

impl Marker {
    pub(crate) fn complete(self, p: &mut Parser<'_>, kind: SyntaxKind) -> CompletedMarker {
        if let Some(event) = p.events.get_mut(self.pos) {
            *event = Event::Start {
                kind,
                forward_parent: None,
            };
        }
        p.events.push(Event::Finish);
        CompletedMarker { pos: self.pos }
    }

    /// Drops the node. Its children stay, in the parent.
    pub(crate) fn abandon(self, p: &mut Parser<'_>) {
        if self.pos.checked_add(1) == Some(p.events.len()) {
            p.events.pop();
        }
    }
}

impl CompletedMarker {
    /// Opens a new node that starts before this one and will contain it.
    pub(crate) fn precede(self, p: &mut Parser<'_>) -> Marker {
        let m = p.start();
        let distance = m
            .pos
            .checked_sub(self.pos)
            .and_then(|d| u32::try_from(d).ok())
            .and_then(NonZeroU32::new);
        if let Some(Event::Start { forward_parent, .. }) = p.events.get_mut(self.pos) {
            *forward_parent = distance;
        }
        m
    }
}

impl<'a> Parser<'a> {
    fn new(file: FileId, lexed: &'a Lexed, text: &'a str) -> Self {
        let ends_in_string = lexed
            .errors()
            .iter()
            .any(|d| d.code == codes::UNTERMINATED_STRING);
        Self {
            file,
            input: Input::new(lexed, text),
            lexed,
            events: Vec::with_capacity(lexed.len().saturating_mul(2)),
            errors: Vec::new(),
            last_error_raw: None,
            prev_end: 0,
            depth: 0,
            too_deep_reported: false,
            ends_in_string,
            open_delims: Vec::new(),
            braces: Braces::new(lexed, text),
            unwinding: false,
        }
    }

    // ---- markers ----------------------------------------------------------

    pub(crate) fn start(&mut self) -> Marker {
        let pos = self.events.len();
        self.events.push(Event::Tombstone);
        Marker { pos }
    }

    // ---- lookahead --------------------------------------------------------

    pub(crate) fn current(&self) -> SyntaxKind {
        self.input.current().kind
    }

    pub(crate) fn nth(&self, n: usize) -> SyntaxKind {
        self.input.nth(n)
    }

    pub(crate) fn at(&self, kind: SyntaxKind) -> bool {
        self.current() == kind
    }

    pub(crate) fn at_ts(&self, set: TokenSet) -> bool {
        set.contains(self.current())
    }

    pub(crate) fn nth_at(&self, n: usize, kind: SyntaxKind) -> bool {
        self.nth(n) == kind
    }

    /// Whether the current token is the first of its line.
    pub(crate) fn at_line_start(&self) -> bool {
        self.input.current().line_start
    }

    /// The current token text, for messages.
    pub(crate) fn current_text(&self) -> &'a str {
        self.input.current_text()
    }

    /// A value that changes whenever a token is consumed, for progress
    /// checks.
    pub(crate) fn position(&self) -> (usize, u8) {
        self.input.position()
    }

    // ---- consuming --------------------------------------------------------

    /// Consumes the current token, whatever it is. Does nothing at the end.
    pub(crate) fn bump_any(&mut self) {
        let consumed = self.input.advance(false);
        self.emit(consumed);
    }

    /// Consumes the current token, which the caller checked is `kind`.
    pub(crate) fn bump(&mut self, kind: SyntaxKind) {
        debug_assert_eq!(self.current(), kind, "bump of the wrong kind");
        self.bump_any();
    }

    /// Consumes the current token as a binary operator, so that a line break
    /// after it continues the line (§2.8).
    pub(crate) fn bump_binop(&mut self) {
        let consumed = self.input.advance(true);
        self.emit(consumed);
    }

    pub(crate) fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump_any();
            true
        } else {
            false
        }
    }

    /// Consumes `kind` or reports that it is missing.
    pub(crate) fn expect(&mut self, kind: SyntaxKind) -> bool {
        if self.eat(kind) {
            return true;
        }
        self.expected(kind.describe());
        false
    }

    fn emit(&mut self, consumed: Consumed) {
        match consumed {
            Consumed::Nothing => {}
            Consumed::Whole { raw } => {
                let range = self.lexed.range(raw);
                if self.lexed.kind(raw) != NEWLINE {
                    self.prev_end = range.end();
                }
                self.events.push(Event::Token { raw: raw_u32(raw) });
            }
            Consumed::Part {
                raw,
                kind,
                offset,
                len,
            } => {
                let start = self
                    .lexed
                    .range(raw)
                    .start()
                    .saturating_add(u32::from(offset));
                self.prev_end = start.saturating_add(u32::from(len));
                self.events.push(Event::Split {
                    raw: raw_u32(raw),
                    kind,
                    offset,
                    len,
                });
            }
        }
    }

    // ---- delimiters -------------------------------------------------------

    /// Consumes the opening delimiter `open` (`(`, `[`, `{` or a type `<`)
    /// and enters its newline mode.
    pub(crate) fn open_delim(&mut self, open: SyntaxKind) -> Delim {
        let mode_depth = self.input.mode_depth();
        let stack_depth = self.open_delims.len();
        let range = self.input.current_range();
        let raw = self.input.current().raw;
        self.bump_any();
        let mode = if open == L_BRACE {
            NlMode::Significant
        } else {
            NlMode::Ignore
        };
        self.input.push_mode(mode);
        self.open_delims.push(open);
        Delim {
            mode_depth,
            stack_depth,
            open,
            raw,
            range,
        }
    }

    /// Whether the current token is a `)` or `]` that closes a delimiter
    /// the grammar opened. A body inside it ends there, unclosed, so that
    /// the enclosing list can take its closer.
    pub(crate) fn at_enclosing_closer(&self) -> bool {
        let current = self.current();
        matches!(current, R_PAREN | R_BRACK)
            && self.open_delims.iter().any(|&open| closer(open) == current)
    }

    /// Whether no `}` anywhere later in the file closes the `{` of `delim`
    /// when braces are matched like brackets ([`Braces`]).
    pub(crate) fn never_closed(&self, delim: &Delim) -> bool {
        delim.open == L_BRACE && self.braces.unmatched.binary_search(&delim.raw).is_ok()
    }

    /// Whether the current token is the first character of its line, where
    /// `cheby fmt` puts items and never local functions.
    pub(crate) fn at_column_one(&self) -> bool {
        let raw = self.input.current().raw;
        raw.checked_sub(1)
            .is_none_or(|prev| self.lexed.kind(prev) == NEWLINE)
    }

    /// Whether the current token closes `delim`. A type `<` is also closed
    /// by the first half of `>>` or `>=` (§2.6).
    pub(crate) fn at_close(&self, delim: &Delim) -> bool {
        let close = closer(delim.open);
        let current = self.current();
        current == close || (delim.open == LT && matches!(current, SHR | GE))
    }

    /// Consumes the closing delimiter of `delim`, or reports E0015, and
    /// leaves its newline mode either way.
    pub(crate) fn close_delim(&mut self, delim: Delim) {
        let close = closer(delim.open);
        // The closer is read in the delimiter's mode; the token after it is
        // decided in the enclosing mode, because the input is lazy.
        if self.at_close(&delim) {
            if delim.open == LT {
                let consumed = self.input.split_gt();
                self.emit(consumed);
            } else {
                self.bump_any();
            }
            self.input.truncate_modes(delim.mode_depth);
            self.open_delims.truncate(delim.stack_depth);
            return;
        }
        let open_text = delim.open.fixed_text().unwrap_or_default();
        let close_text = close.fixed_text().unwrap_or_default();
        let mut diag =
            Diagnostic::new(codes::UNCLOSED_DELIMITER, format!("unclosed `{open_text}`"))
                .with_primary(self.error_span(), format!("expected `{close_text}`"))
                .with_secondary(
                    Span::new(self.file, delim.range),
                    format!("this `{open_text}` is never closed"),
                );
        if let Some(range) = self.likely_unclosed_brace(&delim) {
            diag = diag
                .with_secondary(
                    Span::new(self.file, range),
                    "this `{` may be the one missing its `}`",
                )
                .with_note(
                    "the `}` that closes it is indented differently from the line of the `{`",
                );
        }
        self.push_error(diag, true);
        self.input.truncate_modes(delim.mode_depth);
        self.open_delims.truncate(delim.stack_depth);
    }

    /// For a `{` that is never closed, the first `{` inside its body whose
    /// `}` is indented differently ([`Braces::misindented`]): with
    /// formatted code, the `}` taken to close it most likely belongs to an
    /// enclosing brace, and its own `}` is the one missing.
    fn likely_unclosed_brace(&self, delim: &Delim) -> Option<TextRange> {
        if !self.never_closed(delim) {
            return None;
        }
        let current = self.input.current().raw;
        let misindented = &self.braces.misindented;
        let first = misindented.partition_point(|&raw| raw <= delim.raw);
        let raw = *misindented.get(first)?;
        (raw < current).then(|| self.lexed.range(raw))
    }

    // ---- errors -----------------------------------------------------------

    /// The span an error at the current token points at: the token, or an
    /// empty span after the previous token for a line break or the end.
    fn error_span(&self) -> Span {
        let range = match self.current() {
            NEWLINE | EOF => TextRange::empty(self.prev_end),
            _ => self.input.current_range(),
        };
        Span::new(self.file, range)
    }

    /// Records `diag` unless an error was already reported at this token,
    /// or the token is an error token the lexer reported (unless `always`).
    fn push_error(&mut self, diag: Diagnostic, always: bool) {
        let current = self.input.current();
        if self.last_error_raw == Some(current.raw) {
            return;
        }
        if !always && current.kind == ERROR_TOKEN {
            return;
        }
        if current.kind == EOF && self.ends_in_string {
            return;
        }
        if self.unwinding {
            return;
        }
        self.last_error_raw = Some(current.raw);
        self.errors.push(diag);
    }

    /// Whether an error was already reported at the current token.
    pub(crate) fn error_at_current(&self) -> bool {
        self.last_error_raw == Some(self.input.current().raw)
    }

    /// Reports a finished diagnostic, positioned by the caller. It does not
    /// count as the error at the current token.
    pub(crate) fn report(&mut self, diag: Diagnostic) {
        self.errors.push(diag);
    }

    /// The span of the current token, for diagnostics built by the caller.
    pub(crate) fn current_span(&self) -> Span {
        self.error_span()
    }

    /// The span of `range` in this file.
    pub(crate) const fn span(&self, range: TextRange) -> Span {
        Span::new(self.file, range)
    }

    /// The range of the current token.
    pub(crate) fn current_range(&self) -> TextRange {
        self.input.current_range()
    }

    /// Reports "expected `what`, found …" at the current token. A line
    /// break gets E0014 with the continuation rules (§2.8).
    pub(crate) fn expected(&mut self, what: &str) {
        let found = self.current();
        let diag = if found == NEWLINE {
            Diagnostic::new(codes::UNEXPECTED_LINE_BREAK, "unexpected line break")
                .with_primary(self.error_span(), format!("expected {what}"))
                .with_note(
                    "a line continues only inside `(`, `[` or type `<…>`, after an operator, \
                     `,`, `=`, `=>`, `->` or `<-`, or before a line starting with `|>`, `&&` or \
                     `||`",
                )
        } else {
            Diagnostic::new(
                codes::SYNTAX_ERROR,
                format!("expected {what}, found {}", found.describe()),
            )
            .with_primary(self.error_span(), "")
        };
        self.push_error(diag, false);
    }

    // ---- recovery ---------------------------------------------------------

    /// Skips tokens into an `ERROR` node until the current token is in
    /// `stop` outside any delimiter opened while skipping, or is an item
    /// keyword at the start of a line (if `items`), or the end. Delimiters
    /// are skipped as balanced groups, entering their newline modes. Does
    /// nothing, and makes no node, if already at a stop.
    pub(crate) fn skip_until(&mut self, stop: TokenSet, items: bool) {
        let m = self.start();
        let entry = self.input.mode_depth();
        let mut open: Vec<SyntaxKind> = Vec::new();
        let mut skipped = false;
        loop {
            let kind = self.current();
            if kind == EOF {
                break;
            }
            if items && self.at_item_recovery() {
                break;
            }
            if open.is_empty() && stop.contains(kind) {
                break;
            }
            match kind {
                L_PAREN | L_BRACK | L_BRACE => {
                    let mode = if kind == L_BRACE {
                        NlMode::Significant
                    } else {
                        NlMode::Ignore
                    };
                    self.bump_any();
                    self.input.push_mode(mode);
                    open.push(kind);
                }
                R_PAREN | R_BRACK | R_BRACE => {
                    // A closer of a group opened here closes it (and any
                    // group inside it left open). Another one belongs to an
                    // enclosing construct: stop there if it is a stop
                    // token or closes a delimiter the grammar opened, or
                    // skip it as stray.
                    let Some(at) = open.iter().rposition(|&o| closer(o) == kind) else {
                        if stop.contains(kind)
                            || self.open_delims.iter().any(|&o| closer(o) == kind)
                        {
                            break;
                        }
                        self.bump_any();
                        skipped = true;
                        continue;
                    };
                    self.bump_any();
                    open.truncate(at);
                    let depth = entry.saturating_add(open.len());
                    self.input.truncate_modes(depth);
                }
                _ => self.bump_any(),
            }
            skipped = true;
        }
        self.input.truncate_modes(entry);
        if skipped {
            m.complete(self, ERROR);
        } else {
            m.abandon(self);
        }
    }

    /// Consumes the current token into an `ERROR` node (used to make
    /// progress where nothing else applies).
    pub(crate) fn bump_error(&mut self) {
        if self.at(EOF) {
            return;
        }
        let m = self.start();
        self.bump_any();
        m.complete(self, ERROR);
    }

    /// Whether the current token is an item keyword at the start of a line,
    /// where item-level recovery resumes (§4.6 of the design). `fn` counts
    /// only before a name: `fn(` starts a closure.
    pub(crate) fn at_item_recovery(&self) -> bool {
        self.at_line_start()
            && self.at_ts(recovery::ITEM_RECOVERY)
            && (!self.at(FN_KW) || self.nth_at(1, LOWER_IDENT))
    }

    // ---- depth limit ------------------------------------------------------

    /// Enters one level of nesting. Above [`MAX_DEPTH`], reports E0016 once,
    /// skips to the closing delimiter of the current construct, an item
    /// keyword at the start of a line, or the end, and returns `false`; the
    /// caller then returns without parsing. Until the next top-level item,
    /// further errors are consequences and are not reported.
    pub(crate) fn enter(&mut self) -> bool {
        if self.depth < MAX_DEPTH {
            self.depth = self.depth.saturating_add(1);
            return true;
        }
        if !self.too_deep_reported {
            self.too_deep_reported = true;
            let diag = Diagnostic::new(codes::NESTING_TOO_DEEP, "code nested too deeply")
                .with_primary(self.error_span(), "")
                .with_note(format!("the parser allows {MAX_DEPTH} levels of nesting"))
                .with_help(
                    "name intermediate results with `let`, or split the code into functions",
                );
            self.push_error(diag, true);
        }
        self.unwinding = true;
        let m = self.start();
        let mut open = 0_u32;
        let mut skipped = false;
        loop {
            if open == 0 && self.at_item_recovery() {
                break;
            }
            match self.current() {
                EOF => break,
                L_PAREN | L_BRACK | L_BRACE => open = open.saturating_add(1),
                R_PAREN | R_BRACK | R_BRACE => {
                    if open == 0 {
                        break;
                    }
                    open = open.saturating_sub(1);
                }
                _ => {}
            }
            self.bump_any();
            skipped = true;
        }
        if skipped {
            m.complete(self, ERROR);
        } else {
            m.abandon(self);
        }
        false
    }

    pub(crate) fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Reports errors again after the depth limit stopped parsing: called
    /// at the start of each top-level item.
    pub(crate) fn end_unwinding(&mut self) {
        self.unwinding = false;
    }
}

/// The closing delimiter of `open`.
const fn closer(open: SyntaxKind) -> SyntaxKind {
    match open {
        L_PAREN => R_PAREN,
        L_BRACK => R_BRACK,
        L_BRACE => R_BRACE,
        _ => GT,
    }
}

/// A raw token index as stored in events. Lexed files come from a
/// `SourceMap`, so the index fits.
fn raw_u32(raw: usize) -> u32 {
    u32::try_from(raw).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
