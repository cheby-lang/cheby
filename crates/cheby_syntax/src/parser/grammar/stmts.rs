//! Blocks and statements (App. A.4), and bodies whose entries are separated
//! by line breaks.
//!
//! A statement ends at a significant line break or at the `}` of its block
//! (§2.8). Shapes that `tree-sitter-cheby` accepts but the spec does not,
//! such as a local function with `pub` or without a body, a `let … as msg`
//! without `assert`, or a trailing comma after `use` binders, are left to
//! validation (D-424).

use super::exprs::expr;
use super::items::{fn_rest, visibility};
use super::patterns::pattern;
use super::types::type_;
use crate::SyntaxKind::{
    self, AS_KW, ASSERT_KW, ASSERT_STMT, BLOCK, CASE_BODY, COLON, COMMA, ELSE_CLAUSE, ELSE_KW, EOF,
    EQ, FN_KW, GUARD_KW, GUARD_STMT, L_BRACE, LEFT_ARROW, LET_KW, LET_STMT, LOCAL_FN, LOWER_IDENT,
    MESSAGE, NEWLINE, PRIV_KW, PUB_KW, R_BRACE, USE_BINDER, USE_KW, USE_STMT,
};
use crate::TokenSet;
use crate::parser::recovery::{BLOCK_BREAK, EXPR_FIRST, STMT_STOP};
use crate::parser::{CompletedMarker, Parser};

/// The keywords that start a statement.
const STMT_KEYWORDS: TokenSet = TokenSet::new(&[LET_KW, USE_KW, ASSERT_KW, GUARD_KW]);

/// `{ stmt NL stmt … }`. Returns `None` only when nesting is too deep.
///
/// An item keyword other than `fn` at the start of a line ends the block,
/// which is then unclosed (E0015), so that one missing `}` does not swallow
/// the rest of the file. So does `fn name` in column 1 when no `}` later in
/// the file can close the block (see [`line_body`]).
pub(super) fn block(p: &mut Parser<'_>) -> Option<CompletedMarker> {
    if !p.enter() {
        return None;
    }
    let done = block_expr(p);
    p.leave();
    Some(done)
}

/// A block as an expression, whose nesting the expression already counts.
pub(super) fn block_expr(p: &mut Parser<'_>) -> CompletedMarker {
    line_body(p, BLOCK, EXPR_FIRST, "a statement", stmt)
}

/// A `{ … }` body of entries separated by significant line breaks: blocks,
/// `case` bodies and interface bodies. `entry` is called at each token that
/// is in `first` or starts a statement, and must consume a token.
///
/// When the closing `}` is missing, the body must not swallow the items
/// after it. It ends, unclosed (E0015), at the first line that starts with
/// an item keyword that cannot start an entry ([`at_block_break`]), and at
/// a `)` or `]` that closes an enclosing delimiter. A `case` body also ends
/// at `fn name`, `pub`, `priv` or a statement keyword at the start of a
/// line, which can never start an arm, so the enclosing block goes on with
/// that statement (D-1381). In a block or an interface body, `fn name`
/// starts a local or interface function, so it ends the body only in
/// column 1, where `cheby fmt` puts items, and only when no `}` later in
/// the file can close the body. Valid code always has that `}`, so this
/// never rejects a valid program.
pub(super) fn line_body(
    p: &mut Parser<'_>,
    kind: SyntaxKind,
    first: TokenSet,
    what: &str,
    mut entry: impl FnMut(&mut Parser<'_>),
) -> CompletedMarker {
    let m = p.start();
    let delim = p.open_delim(L_BRACE);
    let starts = |p: &Parser<'_>| {
        p.at_ts(first) || (kind == BLOCK && (p.at_ts(STMT_KEYWORDS) || at_local_fn(p)))
    };
    let ends = |p: &Parser<'_>| {
        p.at(R_BRACE)
            || p.at(EOF)
            || (kind == CASE_BODY && p.at_line_start() && p.at_ts(STMT_KEYWORDS))
            || (at_block_break(p) && !p.at_member_attr(first))
            || p.at_enclosing_closer()
            || (p.at_item_recovery()
                && (kind == CASE_BODY || (p.at_column_one() && p.never_closed(&delim))))
    };
    // A `use` nests the rest of its block in a callback (§5.10), a guard
    // in an arm (§5.18), and later passes recurse into it, so each counts
    // as a level of nesting until the end of the block (D-441, D-519,
    // D-1341).
    let mut nested = 0_u32;
    loop {
        while p.eat(NEWLINE) {}
        if ends(p) {
            break;
        }
        let before = p.position();
        if starts(p) {
            let nests = kind == BLOCK && at_nesting_stmt(p);
            entry(p);
            if nests && p.enter() {
                nested = nested.saturating_add(1);
            }
        } else {
            p.expected(what);
            p.skip_until(STMT_STOP, true);
        }
        if p.at(R_BRACE) {
            break;
        }
        // A line break where the entry could not end (E0014, reported at
        // this line break) cuts it. The next line is then most likely its
        // continuation, such as `= price + tax` or `y: 2 }`, and is skipped
        // without a second error. In a block, a line that starts with a
        // statement keyword or a local function is kept; in other bodies,
        // a line that can start an entry.
        let cut = p.at(NEWLINE) && p.error_at_current();
        if p.eat(NEWLINE) {
            if cut {
                while p.eat(NEWLINE) {}
                let keeps = if kind == BLOCK {
                    p.at_ts(STMT_KEYWORDS) || at_local_fn(p)
                } else {
                    starts(p)
                };
                if !ends(p) && !keeps {
                    p.skip_until(STMT_STOP, true);
                }
            }
            continue;
        }
        if ends(p) {
            break;
        }
        // A failed entry that stopped at a statement at the start of a
        // line stopped at the next entry, not at junk (D-1381).
        if kind == BLOCK
            && p.error_at_current()
            && p.at_line_start()
            && (p.at_ts(STMT_KEYWORDS) || at_local_fn(p))
        {
            continue;
        }
        p.expected("a line break or `}`");
        p.skip_until(STMT_STOP, true);
        if p.position() == before {
            p.bump_error();
        }
    }
    for _ in 0..nested {
        p.leave();
    }
    p.close_delim(delim);
    m.complete(p, kind)
}

/// Whether the statement here consumes the rest of its block: a `use`
/// or a guard.
fn at_nesting_stmt(p: &Parser<'_>) -> bool {
    p.at(USE_KW) || p.at(GUARD_KW) || at_let_guard(p)
}

/// `let guard` (§5.18).
fn at_let_guard(p: &Parser<'_>) -> bool {
    p.at(LET_KW) && p.nth_at(1, GUARD_KW)
}

/// Whether a line starts with a keyword that cannot start a statement.
fn at_block_break(p: &Parser<'_>) -> bool {
    p.at_line_start()
        && (p.at_ts(BLOCK_BREAK) || ((p.at(PUB_KW) || p.at(PRIV_KW)) && !p.nth_at(1, FN_KW)))
}

/// `fn name`, `pub fn` or `priv fn`: a local function. `fn (` is a
/// closure.
fn at_local_fn(p: &Parser<'_>) -> bool {
    (p.at(FN_KW) && p.nth_at(1, LOWER_IDENT))
        || ((p.at(PUB_KW) || p.at(PRIV_KW)) && p.nth_at(1, FN_KW))
}

fn stmt(p: &mut Parser<'_>) {
    match p.current() {
        GUARD_KW => guard_stmt(p),
        LET_KW if at_let_guard(p) => let_guard_stmt(p),
        LET_KW => let_stmt(p),
        USE_KW => use_stmt(p),
        ASSERT_KW => {
            let m = p.start();
            p.bump(ASSERT_KW);
            expr(p);
            if p.at(AS_KW) {
                message(p);
            }
            m.complete(p, ASSERT_STMT);
        }
        _ if at_local_fn(p) => {
            // Parsed like a top-level function; visibility, type
            // parameters and a missing body are rejected by validation.
            let m = p.start();
            if !p.at(FN_KW) {
                visibility(p);
            }
            fn_rest(p);
            m.complete(p, LOCAL_FN);
        }
        _ => {
            expr(p);
        }
    }
}

/// `let [assert] pattern [: Type] = expr [as message]` (§5.2, D-159).
fn let_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(LET_KW);
    p.eat(ASSERT_KW);
    pattern(p);
    if p.eat(COLON) {
        type_(p);
    }
    if p.expect(EQ) {
        expr(p);
        if p.at(AS_KW) {
            message(p);
        }
    }
    m.complete(p, LET_STMT);
}

/// `guard cond else { … }` (§5.18, D-1340).
fn guard_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(GUARD_KW);
    expr(p);
    else_clause(p);
    m.complete(p, GUARD_STMT);
}

/// `let guard pattern [: Type] = expr else { … }` (§5.18, D-1340).
fn let_guard_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(LET_KW);
    p.bump(GUARD_KW);
    pattern(p);
    if p.eat(COLON) {
        type_(p);
    }
    if p.expect(EQ) {
        expr(p);
        else_clause(p);
    }
    m.complete(p, GUARD_STMT);
}

/// `else { … }`, with `else` and `{` on the line where the guard's
/// condition or value ends (§2.8, D-1341).
fn else_clause(p: &mut Parser<'_>) {
    if !p.at(ELSE_KW) {
        p.expected(ELSE_KW.describe());
        return;
    }
    let m = p.start();
    p.bump(ELSE_KW);
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.expected(L_BRACE.describe());
    }
    m.complete(p, ELSE_CLAUSE);
}

/// `use a, b: T <- f(x)` (§5.10).
fn use_stmt(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(USE_KW);
    if !p.at(LEFT_ARROW) {
        use_binder(p);
        while p.eat(COMMA) {
            if p.at(LEFT_ARROW) {
                break;
            }
            use_binder(p);
        }
    }
    if p.expect(LEFT_ARROW) {
        expr(p);
    }
    m.complete(p, USE_STMT);
}

fn use_binder(p: &mut Parser<'_>) {
    let m = p.start();
    pattern(p);
    if p.eat(COLON) {
        type_(p);
    }
    m.complete(p, USE_BINDER);
}

/// `as expr`, the message of `let assert`, `assert`, `panic` and `todo`
/// (D-159, D-160, D-161).
pub(super) fn message(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(AS_KW);
    expr(p);
    m.complete(p, MESSAGE);
}
