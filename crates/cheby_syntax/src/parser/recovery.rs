//! Token sets for lookahead and error recovery (§4.6 of the phase-A design).

use crate::SyntaxKind::{
    AS_KW, ASSERT_KW, AT, AT_BANG, BANG, CASE_KW, COMMA, CONST_KW, DYN_KW, EOF, EQ, EXPOSED_KW,
    FAT_ARROW, FLOAT_NUMBER, FN_KW, IMPORT_KW, INT_NUMBER, INTERFACE_KW, L_BRACE, L_BRACK, L_PAREN,
    LET_KW, LOWER_IDENT, MINUS, NEWLINE, PANIC_KW, PRIV_KW, PUB_KW, R_BRACE, R_BRACK, R_PAREN,
    RAW_STRING, STRING_START, TEST_KW, TODO_KW, TYPE_KW, UNDERSCORE, UPPER_IDENT, USE_KW, WHEN_KW,
};
use crate::TokenSet;

/// Tokens where item-level recovery resumes, when they start a line.
pub(super) const ITEM_RECOVERY: TokenSet = TokenSet::new(&[
    FN_KW,
    TYPE_KW,
    CONST_KW,
    INTERFACE_KW,
    TEST_KW,
    IMPORT_KW,
    PUB_KW,
    PRIV_KW,
    EXPOSED_KW,
    AT,
    AT_BANG,
]);

/// Tokens that start an item body (after attributes).
pub(super) const ITEM_BODY_FIRST: TokenSet = TokenSet::new(&[
    FN_KW,
    TYPE_KW,
    CONST_KW,
    INTERFACE_KW,
    TEST_KW,
    PUB_KW,
    PRIV_KW,
    EXPOSED_KW,
]);

/// Item keywords that end a block when they start a line: everything that
/// cannot start a statement. `fn`, `pub fn` and `priv fn` start local
/// functions and are checked separately.
pub(super) const BLOCK_BREAK: TokenSet = TokenSet::new(&[
    TYPE_KW,
    CONST_KW,
    INTERFACE_KW,
    TEST_KW,
    IMPORT_KW,
    EXPOSED_KW,
    AT,
    AT_BANG,
]);

/// Tokens that can start a type (App. A.3).
pub(super) const TYPE_FIRST: TokenSet =
    TokenSet::new(&[UPPER_IDENT, LOWER_IDENT, L_PAREN, FN_KW, DYN_KW]);

/// Tokens that can start a pattern (App. A.6).
pub(super) const PAT_FIRST: TokenSet = TokenSet::new(&[
    UNDERSCORE,
    LOWER_IDENT,
    UPPER_IDENT,
    MINUS,
    INT_NUMBER,
    FLOAT_NUMBER,
    STRING_START,
    RAW_STRING,
    L_PAREN,
    L_BRACK,
]);

/// Tokens that can start an expression (App. A.5). `_` starts a hole.
pub(super) const EXPR_FIRST: TokenSet = TokenSet::new(&[
    LOWER_IDENT,
    UPPER_IDENT,
    INT_NUMBER,
    FLOAT_NUMBER,
    RAW_STRING,
    STRING_START,
    L_PAREN,
    L_BRACK,
    L_BRACE,
    FN_KW,
    CASE_KW,
    PANIC_KW,
    TODO_KW,
    MINUS,
    BANG,
    UNDERSCORE,
]);

/// Tokens at which a missing expression, pattern or type is reported
/// without consuming anything: they belong to an enclosing construct.
pub(super) const EXPR_RECOVERY: TokenSet = TokenSet::new(&[
    NEWLINE, EOF, COMMA, R_PAREN, R_BRACK, R_BRACE, L_BRACE, EQ, FAT_ARROW, WHEN_KW, AS_KW, LET_KW,
    USE_KW, ASSERT_KW,
]);

/// Where a comma list stops when it meets a token it cannot use: closers
/// of enclosing lists, statement keywords, and `=>` and `=`, which never
/// stand directly in a list and so belong to the construct around it (a
/// `case` arm, a `let`). Brace lists also stop at a significant line break.
pub(super) const LIST_STOP: TokenSet = TokenSet::new(&[
    R_PAREN, R_BRACK, R_BRACE, LET_KW, USE_KW, ASSERT_KW, FAT_ARROW, EQ, EOF,
]);

/// Where a statement-level skip stops.
pub(super) const STMT_STOP: TokenSet = TokenSet::new(&[NEWLINE, R_BRACE]);
