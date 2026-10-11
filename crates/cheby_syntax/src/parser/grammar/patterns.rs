//! Patterns (App. A.6, ch. 6).
//!
//! `pattern = alt_pattern ["as" LOWER]` with `alt_pattern = primary {"|"
//! primary}`. Like `tree-sitter-cheby`, the parser also accepts `|` and
//! `as` after an `as` pattern, left to right (`A as x | B`, `p as a as b`),
//! and leaves those shapes to validation (D-424, D-438). The `|` of an
//! or-pattern is not a binary operator, so a line break after it ends the
//! line (§2.8).

use cheby_diag::{Diagnostic, codes};

use super::exprs::string_lit;
use super::{List, comma_list, list_rest, name, path};
use crate::SyntaxKind::{
    AS_KW, AS_PAT, COLON, COLON2, CTOR_PAT, DOT2, FIELD_PAT, FIELD_PAT_LIST, FLOAT_NUMBER,
    IDENT_PAT, INT_NUMBER, L_BRACE, L_BRACK, L_PAREN, LIST_PAT, LITERAL_PAT, LOWER_IDENT, MINUS,
    OR_PAT, PAREN_PAT, PIPE, POS_PAT_LIST, RAW_STRING, REST_PAT, STRING_START, TUPLE_PAT,
    UNDERSCORE, UPPER_IDENT, WILDCARD_PAT,
};
use crate::TokenSet;
use crate::parser::recovery::{EXPR_RECOVERY, PAT_FIRST};
use crate::parser::{CompletedMarker, Parser};

/// A pattern. Reports and returns `None` if there is none here.
pub(super) fn pattern(p: &mut Parser<'_>) -> Option<CompletedMarker> {
    if !p.at_ts(PAT_FIRST) {
        p.expected("a pattern");
        if !p.at_ts(EXPR_RECOVERY) && !p.at_item_recovery() {
            p.bump_error();
        }
        return None;
    }
    if !p.enter() {
        return None;
    }
    let mut lhs = primary(p);
    loop {
        if p.at(PIPE) {
            let m = lhs.precede(p);
            while p.eat(PIPE) {
                if p.at_ts(PAT_FIRST) {
                    primary(p);
                } else {
                    p.expected("a pattern");
                }
            }
            lhs = m.complete(p, OR_PAT);
        } else if p.at(AS_KW) {
            let m = lhs.precede(p);
            p.bump(AS_KW);
            name(p, LOWER_IDENT, "a name");
            lhs = m.complete(p, AS_PAT);
        } else {
            break;
        }
    }
    p.leave();
    Some(lhs)
}

/// A pattern without `|` or `as`. The caller checked `PAT_FIRST`.
fn primary(p: &mut Parser<'_>) -> CompletedMarker {
    match p.current() {
        UPPER_IDENT => ctor(p),
        LOWER_IDENT if p.nth_at(1, COLON2) && p.nth_at(2, UPPER_IDENT) => ctor(p),
        LOWER_IDENT => single(p, IDENT_PAT),
        UNDERSCORE => single(p, WILDCARD_PAT),
        INT_NUMBER | FLOAT_NUMBER | RAW_STRING => single(p, LITERAL_PAT),
        MINUS => {
            // A negative number. Float patterns are rejected by validation
            // (E0033).
            let m = p.start();
            p.bump(MINUS);
            if p.at(INT_NUMBER) || p.at(FLOAT_NUMBER) {
                p.bump_any();
            } else {
                p.expected("a number");
            }
            m.complete(p, LITERAL_PAT)
        }
        STRING_START => {
            // Interpolation is rejected by validation (E0034).
            let m = p.start();
            string_lit(p);
            m.complete(p, LITERAL_PAT)
        }
        L_BRACK => list(p),
        _ => paren_or_tuple(p),
    }
}

fn single(p: &mut Parser<'_>, kind: crate::SyntaxKind) -> CompletedMarker {
    let m = p.start();
    p.bump_any();
    m.complete(p, kind)
}

/// `Name`, `module::Name`, `Name(p, …)` or `Name { field: p, .. }`.
fn ctor(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    path(p, false);
    if p.at(L_PAREN) {
        let list = List {
            open: L_PAREN,
            kind: POS_PAT_LIST,
            first: PAT_FIRST,
            what: "a pattern",
            non_empty: false,
        };
        comma_list(p, &list, |p| {
            pattern(p);
        });
    } else if p.at(L_BRACE) {
        let list = List {
            open: L_BRACE,
            kind: FIELD_PAT_LIST,
            first: TokenSet::new(&[LOWER_IDENT, DOT2]),
            what: "a field pattern",
            non_empty: false,
        };
        comma_list(p, &list, field_pat);
    }
    m.complete(p, CTOR_PAT)
}

/// `name`, `name: pattern` or `..` in a field pattern list. Where the rest
/// stands is checked by validation (E0032); a name after it is rejected
/// here, as `tree-sitter-cheby` does.
fn field_pat(p: &mut Parser<'_>) {
    let m = p.start();
    if p.at(DOT2) {
        p.bump(DOT2);
        if p.at(LOWER_IDENT) {
            let diag = Diagnostic::new(
                codes::MISPLACED_REST,
                "the rest of a field pattern list takes no name",
            )
            .with_primary(p.current_span(), "")
            .with_help("write `..` alone; it ignores the remaining fields");
            p.report(diag);
            p.bump(LOWER_IDENT);
        }
        m.complete(p, REST_PAT);
        return;
    }
    p.bump(LOWER_IDENT);
    if p.eat(COLON) {
        pattern(p);
    }
    m.complete(p, FIELD_PAT);
}

/// `[p, ..rest, q]`. More than one rest is rejected by validation (E0032).
fn list(p: &mut Parser<'_>) -> CompletedMarker {
    let list = List {
        open: L_BRACK,
        kind: LIST_PAT,
        first: PAT_FIRST.union(TokenSet::new(&[DOT2])),
        what: "a pattern",
        non_empty: false,
    };
    comma_list(p, &list, |p| {
        if p.at(DOT2) {
            let m = p.start();
            p.bump(DOT2);
            p.eat(LOWER_IDENT);
            m.complete(p, REST_PAT);
        } else {
            pattern(p);
        }
    })
}

/// `(p)` is a parenthesized pattern; `()`, `(p,)` and `(p, q, …)` are tuple
/// patterns, and fewer than two elements is E0030 (V).
fn paren_or_tuple(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    let delim = p.open_delim(L_PAREN);
    let list = List {
        open: L_PAREN,
        kind: TUPLE_PAT,
        first: PAT_FIRST,
        what: "a pattern",
        non_empty: false,
    };
    if p.at_close(&delim) {
        p.close_delim(delim);
        return m.complete(p, TUPLE_PAT);
    }
    if p.at_ts(PAT_FIRST) {
        pattern(p);
        if p.at_close(&delim) {
            p.close_delim(delim);
            return m.complete(p, PAREN_PAT);
        }
        list_rest(p, delim, &list, 1, true, |p| {
            pattern(p);
        });
    } else {
        list_rest(p, delim, &list, 0, false, |p| {
            pattern(p);
        });
    }
    m.complete(p, TUPLE_PAT)
}
