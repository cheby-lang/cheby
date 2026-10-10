//! Expressions (App. A.5, ch. 5).
//!
//! Binary operators are parsed by precedence climbing with the levels of
//! §5.4.1 (D-108), all left-associative. Comparisons are parsed
//! left-associatively too, as in `tree-sitter-cheby`; a chain such as
//! `a < b < c` is rejected by validation (E0029). Every binary operator is
//! read with [`Parser::bump_binop`], so a line break after it continues the
//! line (§2.8). Prefix `-` and `!` are not binary operators.

use cheby_diag::{Diagnostic, TextRange, TextSize, codes};

use super::items::{param_list, ret_type};
use super::patterns::pattern;
use super::stmts::{block, block_expr, line_body, message};
use super::types::type_arg_list;
use super::{List, PathEnd, comma_list, list_rest, path};
use crate::SyntaxKind::{
    self, AMP, AMP2, ARG_LIST, AS_KW, BANG, BIN_EXPR, CALL_EXPR, CARET, CASE_ARM, CASE_BODY,
    CASE_EXPR, CASE_KW, CLOSURE, COLON, COLON_QUESTION, COLON2, COMMA, COND_ARM, DOT, DOT2, EOF,
    EQ2, ERROR, ERROR_TOKEN, FAT_ARROW, FIELD_EXPR, FIELD_INIT, FLOAT_NUMBER, FN_KW, GE, GT, GUARD,
    HOLE, INT_NUMBER, INTERP_END, INTERP_START, INTERPOLATION, L_BRACE, L_BRACK, L_PAREN, LE,
    LIST_EXPR, LITERAL, LOWER_IDENT, LT, MINUS, NEQ, PANIC_EXPR, PANIC_KW, PAREN_EXPR, PATH_EXPR,
    PERCENT, PIPE, PIPE_GT, PIPE2, PLUS, PREFIX_EXPR, R_PAREN, RAW_STRING, RECORD_EXPR, SHL, SHR,
    SLASH, SPREAD, STAR, STRING_END, STRING_ESCAPE, STRING_LIT, STRING_START, STRING_TEXT,
    THIN_ARROW, TODO_EXPR, TODO_KW, TUPLE_EXPR, UNDERSCORE, UPPER_IDENT, WHEN_KW,
};
use crate::TokenSet;
use crate::parser::recovery::{EXPR_FIRST, EXPR_RECOVERY, PAT_FIRST};
use crate::parser::{CompletedMarker, Parser};

/// What an expression may not contain at its top level.
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Restrictions {
    /// In a `case` subject, a path ending in a type name followed by `{`
    /// is a path, and the `{` opens the arms (§5.9.6).
    no_record: bool,
}

/// The binding power of unary `-` and `!` (§5.4.1): above every binary
/// operator, below calls and field access.
const PREFIX_BP: u8 = 11;

/// The binding power of a binary operator, or `None`.
const fn infix_bp(kind: SyntaxKind) -> Option<u8> {
    let bp = match kind {
        PIPE2 => 1,
        AMP2 => 2,
        EQ2 | NEQ | LT | LE | GT | GE => 3,
        PIPE_GT => 4,
        PIPE => 5,
        CARET => 6,
        AMP => 7,
        SHL | SHR => 8,
        PLUS | MINUS => 9,
        STAR | SLASH | PERCENT => 10,
        _ => return None,
    };
    Some(bp)
}

/// An expression. Reports and returns `None` if there is none here.
pub(super) fn expr(p: &mut Parser<'_>) -> Option<CompletedMarker> {
    expr_with(p, Restrictions::default())
}

fn expr_with(p: &mut Parser<'_>, r: Restrictions) -> Option<CompletedMarker> {
    if !p.at_ts(EXPR_FIRST) && !p.at(ERROR_TOKEN) {
        missing_expr(p);
        return None;
    }
    expr_bp(p, 1, r)
}

/// Reports a missing expression, and skips the current token unless an
/// enclosing construct can use it.
fn missing_expr(p: &mut Parser<'_>) {
    p.expected("an expression");
    if !p.at_ts(EXPR_RECOVERY) && !p.at_item_recovery() {
        p.bump_error();
    }
}

/// Operators with binding power at least `min_bp`, left-associative.
fn expr_bp(p: &mut Parser<'_>, min_bp: u8, r: Restrictions) -> Option<CompletedMarker> {
    if !p.enter() {
        return None;
    }
    let Some(mut lhs) = unary(p, r) else {
        p.leave();
        return None;
    };
    // Each operator of a chain nests the expression before it one level
    // deeper (`a + b + c` is `(a + b) + c`), and later passes recurse
    // along the chain, so a finished operation counts as a level for the
    // rest of the chain (D-441). Its own right operand is parsed before
    // it counts, so right-nested code costs no more than before (D-519).
    let mut levels = 0_u32;
    while let Some(bp) = infix_bp(p.current()) {
        if bp < min_bp {
            break;
        }
        let m = lhs.precede(p);
        p.bump_binop();
        expr_bp(p, bp.saturating_add(1), r);
        lhs = m.complete(p, BIN_EXPR);
        if !p.enter() {
            break;
        }
        levels = levels.saturating_add(1);
    }
    for _ in 0..levels {
        p.leave();
    }
    p.leave();
    Some(lhs)
}

fn unary(p: &mut Parser<'_>, r: Restrictions) -> Option<CompletedMarker> {
    // A character the lexer rejected (E0003) is skipped, so that `$x`
    // still parses as `x`.
    while p.at(ERROR_TOKEN) {
        p.bump_error();
    }
    if p.at(MINUS) || p.at(BANG) {
        let m = p.start();
        p.bump_any();
        if p.at_ts(EXPR_FIRST) {
            expr_bp(p, PREFIX_BP, r);
        } else {
            missing_expr(p);
        }
        return Some(m.complete(p, PREFIX_EXPR));
    }
    if !p.at_ts(EXPR_FIRST) {
        missing_expr(p);
        return None;
    }
    let mut lhs = primary(p, r)?;
    // Like an operator chain, each call or field access nests the
    // expression before it one level deeper, and counts for the rest of
    // the chain once it is finished (D-441, D-519).
    let mut levels = 0_u32;
    while matches!(p.current(), L_PAREN | DOT) {
        let m = lhs.precede(p);
        if p.at(L_PAREN) {
            arg_list(p);
            lhs = m.complete(p, CALL_EXPR);
        } else {
            p.bump(DOT);
            field_name(p);
            lhs = m.complete(p, FIELD_EXPR);
        }
        if !p.enter() {
            break;
        }
        levels = levels.saturating_add(1);
    }
    for _ in 0..levels {
        p.leave();
    }
    Some(lhs)
}

/// The name after `.`. A number there is positional access, which Cheby
/// does not have (E0024, D-241).
fn field_name(p: &mut Parser<'_>) {
    match p.current() {
        LOWER_IDENT => p.bump(LOWER_IDENT),
        INT_NUMBER | FLOAT_NUMBER => {
            let diag = Diagnostic::new(
                codes::TUPLE_INDEX,
                format!("positional access `.{}` is not allowed", p.current_text()),
            )
            .with_primary(p.current_span(), "")
            .with_help("read the tuple with a pattern, such as `let (first, second) = pair`");
            p.report(diag);
            p.bump_any();
        }
        UPPER_IDENT => {
            p.expected("a field name");
            p.bump(UPPER_IDENT);
        }
        _ => p.expected("a field name"),
    }
}

fn primary(p: &mut Parser<'_>, r: Restrictions) -> Option<CompletedMarker> {
    let done = match p.current() {
        INT_NUMBER | FLOAT_NUMBER | RAW_STRING => {
            let m = p.start();
            p.bump_any();
            m.complete(p, LITERAL)
        }
        STRING_START => string_lit(p),
        LOWER_IDENT | UPPER_IDENT => path_or_record(p, r),
        UNDERSCORE => {
            let diag = Diagnostic::new(
                codes::MISPLACED_HOLE,
                "`_` is allowed only as a whole call argument",
            )
            .with_primary(p.current_span(), "")
            .with_help("write a closure instead, such as `fn(x) { x + 1 }`");
            p.report(diag);
            hole(p)
        }
        L_PAREN => paren_or_tuple(p),
        L_BRACK => {
            let list = List {
                open: L_BRACK,
                kind: LIST_EXPR,
                first: EXPR_FIRST.union(TokenSet::new(&[DOT2])),
                what: "an expression",
                non_empty: false,
            };
            comma_list(p, &list, expr_or_spread)
        }
        L_BRACE => block_expr(p),
        FN_KW => closure(p),
        CASE_KW => case_expr(p),
        PANIC_KW | TODO_KW => {
            let kind = if p.at(PANIC_KW) {
                PANIC_EXPR
            } else {
                TODO_EXPR
            };
            let m = p.start();
            p.bump_any();
            if p.at(AS_KW) {
                message(p);
            }
            m.complete(p, kind)
        }
        _ => {
            missing_expr(p);
            return None;
        }
    };
    Some(done)
}

fn hole(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    p.bump(UNDERSCORE);
    m.complete(p, HOLE)
}

/// A path, with a turbofish `::<T>` (§5.17), or a record expression
/// `Name { … }` (§5.11). The record's path may be module-qualified
/// (D-429).
fn path_or_record(p: &mut Parser<'_>, r: Restrictions) -> CompletedMarker {
    let m = p.start();
    let end = path(p, true);
    if p.at(COLON2) && p.nth_at(1, LT) {
        p.bump(COLON2);
        type_arg_list(p);
        return m.complete(p, PATH_EXPR);
    }
    if end == PathEnd::Upper && p.at(L_BRACE) && !r.no_record {
        let list = List {
            open: L_BRACE,
            kind: RECORD_EXPR,
            first: TokenSet::new(&[LOWER_IDENT, DOT2]),
            what: "a field",
            non_empty: false,
        };
        let delim = p.open_delim(L_BRACE);
        // Spreads are accepted anywhere; validation checks the shape
        // (E0031).
        list_rest(p, delim, &list, 0, false, |p| {
            if p.at(DOT2) {
                spread(p);
            } else {
                let m = p.start();
                p.bump(LOWER_IDENT);
                if p.eat(COLON) {
                    expr(p);
                }
                m.complete(p, FIELD_INIT);
            }
        });
        return m.complete(p, RECORD_EXPR);
    }
    m.complete(p, PATH_EXPR)
}

fn expr_or_spread(p: &mut Parser<'_>) {
    if p.at(DOT2) {
        spread(p);
    } else {
        expr(p);
    }
}

/// `..expr` in a list or a record update.
fn spread(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(DOT2);
    expr(p);
    m.complete(p, SPREAD);
}

/// `(e)` is a parenthesized expression, `(a, b, …)` and `(e,)` are tuples.
/// `()` is rejected here (E0030), as in `tree-sitter-cheby`; `(e,)` is left
/// to validation.
fn paren_or_tuple(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    let open = p.current_range();
    let delim = p.open_delim(L_PAREN);
    let list = List {
        open: L_PAREN,
        kind: TUPLE_EXPR,
        first: EXPR_FIRST,
        what: "an expression",
        non_empty: false,
    };
    if p.at_close(&delim) {
        let range = open.cover(p.current_range());
        let diag = Diagnostic::new(codes::SHORT_TUPLE, "`()` is not a value")
            .with_primary(p.span(range), "")
            .with_help("the value that carries no information is `Nil`");
        p.report(diag);
        p.close_delim(delim);
        return m.complete(p, TUPLE_EXPR);
    }
    if !p.at_ts(EXPR_FIRST) {
        list_rest(p, delim, &list, 0, false, |p| {
            expr(p);
        });
        return m.complete(p, TUPLE_EXPR);
    }
    expr(p);
    if p.at_close(&delim) {
        p.close_delim(delim);
        return m.complete(p, PAREN_EXPR);
    }
    list_rest(p, delim, &list, 1, true, |p| {
        expr(p);
    });
    m.complete(p, TUPLE_EXPR)
}

/// `(a, _, c)`. A `_` that is a whole argument is a hole (§5.6); how many
/// holes a call has is checked by validation (E0026, E0027).
fn arg_list(p: &mut Parser<'_>) {
    let list = List {
        open: L_PAREN,
        kind: ARG_LIST,
        first: EXPR_FIRST,
        what: "an argument",
        non_empty: false,
    };
    comma_list(p, &list, |p| {
        if p.at(UNDERSCORE) && matches!(p.nth(1), COMMA | R_PAREN) {
            hole(p);
        } else {
            expr(p);
        }
    });
}

/// `fn(params) -> T { … }` (§5.7). Parameters are patterns (D-242).
fn closure(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    p.bump(FN_KW);
    if p.at(L_PAREN) {
        param_list(p);
    } else {
        p.expected("`(`");
    }
    if p.at(THIN_ARROW) {
        ret_type(p);
    }
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.expected("a block");
    }
    m.complete(p, CLOSURE)
}

/// `case a, b { arms }` or the subjectless `case { cond => … }` (§5.9). A
/// `{` right after `case` always starts the subjectless form.
fn case_expr(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    p.bump(CASE_KW);
    if p.at(L_BRACE) {
        line_body(p, CASE_BODY, EXPR_FIRST, "a condition arm", cond_arm);
        return m.complete(p, CASE_EXPR);
    }
    let subject = Restrictions { no_record: true };
    expr_with(p, subject);
    while p.eat(COMMA) {
        expr_with(p, subject);
    }
    if p.at(L_BRACE) {
        line_body(p, CASE_BODY, PAT_FIRST, "a case arm", case_arm);
    } else {
        p.expected("`{`");
    }
    m.complete(p, CASE_EXPR)
}

/// `pattern, … when guard => expr`.
fn case_arm(p: &mut Parser<'_>) {
    let m = p.start();
    pattern(p);
    while p.eat(COMMA) {
        pattern(p);
    }
    if p.at(WHEN_KW) {
        let guard = p.start();
        p.bump(WHEN_KW);
        expr(p);
        guard.complete(p, GUARD);
    }
    if p.expect(FAT_ARROW) {
        expr(p);
    }
    m.complete(p, CASE_ARM);
}

/// `condition => expr` or `_ => expr`. A bare `_` here is a token, not a
/// hole. Whether the last arm is `_` is checked by validation (E0035).
fn cond_arm(p: &mut Parser<'_>) {
    let m = p.start();
    if p.at(UNDERSCORE) && p.nth_at(1, FAT_ARROW) {
        p.bump(UNDERSCORE);
    } else {
        expr(p);
    }
    if p.expect(FAT_ARROW) {
        expr(p);
    }
    m.complete(p, COND_ARM);
}

/// A string literal with its parts (§2.5.3, §2.5.4). An unterminated string
/// ends at the end of the file; the lexer reported it (E0010).
///
/// Strings may span lines (D-114), so after a missing `"` the rest of the
/// file is string text, and every `{` in it starts an interpolation. The
/// interpolation errors after the string's first line break are only
/// consequences of the missing `"`, and are dropped.
pub(super) fn string_lit(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    let errors_before = p.errors.len();
    let mut first_line_break = None;
    p.bump(STRING_START);
    let terminated = loop {
        match p.current() {
            STRING_TEXT | STRING_ESCAPE => {
                if first_line_break.is_none()
                    && let Some(at) = p.current_text().find('\n')
                {
                    let at = TextSize::try_from(at).unwrap_or(TextSize::MAX);
                    first_line_break = Some(p.current_range().start().saturating_add(at));
                }
                p.bump_any();
            }
            INTERP_START => interpolation(p),
            STRING_END => {
                p.bump(STRING_END);
                break true;
            }
            _ => break false,
        }
    };
    if !terminated && let Some(cut) = first_line_break {
        let later = p.errors.split_off(errors_before.min(p.errors.len()));
        p.errors.extend(
            later
                .into_iter()
                .filter(|d| d.primary_span().is_none_or(|span| span.range.start() < cut)),
        );
    }
    m.complete(p, STRING_LIT)
}

/// Tokens that end the inside of an interpolation.
const INTERP_LIMIT: TokenSet = TokenSet::new(&[
    INTERP_END,
    STRING_TEXT,
    STRING_ESCAPE,
    STRING_END,
    INTERP_START,
    EOF,
]);

/// `{name.field:?}` in a string (§2.5.4, D-115). Anything else between the
/// braces is E0012. Whitespace there is trivia here, and reported by
/// validation.
fn interpolation(p: &mut Parser<'_>) {
    let m = p.start();
    let open = p.current_span();
    p.bump(INTERP_START);
    let mut named = false;
    if p.at(LOWER_IDENT) {
        p.bump(LOWER_IDENT);
        named = true;
        while p.at(DOT) && p.nth_at(1, LOWER_IDENT) {
            p.bump(DOT);
            p.bump(LOWER_IDENT);
        }
        p.eat(COLON_QUESTION);
    }
    if p.at(INTERP_END) {
        if !named {
            let diag = Diagnostic::new(codes::INVALID_INTERPOLATION, "empty string interpolation")
                .with_primary(p.span(open.range.cover(p.current_range())), "")
                .with_help("write a name between the braces, or escape the brace as `\\{`");
            p.report(diag);
        }
        p.bump(INTERP_END);
        m.complete(p, INTERPOLATION);
        return;
    }
    let mut reported = false;
    if !p.at_ts(INTERP_LIMIT) {
        reported = true;
        let found = p.current().describe();
        let diag = Diagnostic::new(
            codes::INVALID_INTERPOLATION,
            format!("unexpected {found} in string interpolation"),
        )
        .with_primary(p.current_span(), "")
        .with_help(
            "an interpolation holds a name, optionally with fields and `:?`, such as \
             `{user.name}`; bind other values to a name first",
        );
        p.report(diag);
        let error = p.start();
        while !p.at_ts(INTERP_LIMIT) {
            p.bump_any();
        }
        error.complete(p, ERROR);
    }
    if p.at(INTERP_END) {
        p.bump(INTERP_END);
    } else if !reported {
        let text = if p.at(STRING_TEXT) {
            p.current_text()
        } else {
            ""
        };
        let diag = if text.starts_with('\n') || text.starts_with("\r\n") {
            // The lexer ends an interpolation at a line break, so a `}` on
            // a later line does not close it.
            let at = p.span(TextRange::empty(p.current_range().start()));
            Diagnostic::new(
                codes::INVALID_INTERPOLATION,
                "line break inside a string interpolation",
            )
            .with_primary(at, "expected `}` before the line break")
            .with_secondary(open, "the interpolation starts here")
            .with_help(
                "whitespace is not allowed inside the braces (§2.5.4, D-115); close the \
                 interpolation with `}` on the same line, or escape the brace as `\\{`",
            )
        } else {
            Diagnostic::new(
                codes::INVALID_INTERPOLATION,
                "unclosed string interpolation",
            )
            .with_primary(open, "this `{` is never closed")
            .with_help("close it with `}`, or escape the brace as `\\{`")
        };
        p.report(diag);
    }
    m.complete(p, INTERPOLATION);
}
