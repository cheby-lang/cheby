//! Items and imports (App. A.1, A.2, ch. 4).
//!
//! An item node starts at its first attribute, so attributes and the line
//! breaks after them are children of the item; doc comments before it are
//! attached by the tree builder. Shapes the spec forbids but
//! `tree-sitter-cheby` accepts, such as `exposed` without `pub`, parameter
//! patterns in top-level functions or bounds on type-declaration
//! parameters, are left to validation (D-424).

use cheby_diag::{Diagnostic, TextRange, codes};

use super::exprs::{expr, string_lit};
use super::stmts::{block, line_body};
use super::types::{bound_list, type_, type_list};
use super::{List, attrs, comma_list, name};
use crate::SyntaxKind::{
    self, ALIAS_DECL, AS_KW, AT, COLON, COLON2, CONST_DECL, CONST_KW, EOF, EQ, ERROR, EXPOSED_KW,
    FN_DECL, FN_KW, IMPORT_DECL, IMPORT_ITEM, IMPORT_KW, IMPORT_LIST, INTERFACE_BODY,
    INTERFACE_DECL, INTERFACE_FN, INTERFACE_KW, L_BRACE, L_PAREN, LOWER_IDENT, LT, MODULE_PATH,
    NEWLINE, PARAM, PARAM_LIST, POS_FIELD_LIST, PRIV_KW, PUB_KW, RAW_STRING, RECORD_FIELD,
    RECORD_FIELD_LIST, RENAME, RET_TYPE, STRING_START, TEST_DECL, TEST_KW, THIN_ARROW, TYPE_DECL,
    TYPE_KW, TYPE_PARAM, TYPE_PARAM_LIST, UPPER_IDENT, VARIANT, VARIANT_LIST, VISIBILITY,
};
use crate::TokenSet;
use crate::parser::Parser;
use crate::parser::grammar::patterns::pattern;
use crate::parser::recovery::{ITEM_BODY_FIRST, PAT_FIRST, TYPE_FIRST};

/// An item with its attributes (App. A.2). Attributes that no item follows
/// become an `ERROR` node without a diagnostic: `tree-sitter-cheby` accepts
/// them, and validation reports E0019.
///
/// Returns whether a line break must follow.
pub(super) fn item(p: &mut Parser<'_>) -> bool {
    let m = p.start();
    while p.at(AT) {
        attrs::attr(p);
        if p.eat(NEWLINE) {
            continue;
        }
        if !p.at(EOF) {
            p.expected("a line break after the attribute");
        }
        if !p.at(AT) && !p.at_ts(ITEM_BODY_FIRST) {
            break;
        }
    }
    if !p.at_ts(ITEM_BODY_FIRST) {
        m.complete(p, ERROR);
        return true;
    }
    let visibility = if p.at(PUB_KW) || p.at(PRIV_KW) {
        let range = p.current_range();
        visibility(p);
        Some(range)
    } else {
        None
    };
    let exposed = if p.at(EXPOSED_KW) {
        let range = p.current_range();
        p.bump(EXPOSED_KW);
        Some(range)
    } else {
        None
    };
    let kind = match p.current() {
        FN_KW => {
            fn_rest(p);
            FN_DECL
        }
        TYPE_KW => type_rest(p),
        CONST_KW => {
            const_rest(p);
            CONST_DECL
        }
        INTERFACE_KW => {
            interface_rest(p);
            INTERFACE_DECL
        }
        TEST_KW => {
            if let Some(range) = visibility {
                let diag = Diagnostic::new(codes::SYNTAX_ERROR, "a test has no visibility")
                    .with_primary(p.span(range), "")
                    .with_help("remove the visibility");
                p.report(diag);
            }
            test_rest(p);
            TEST_DECL
        }
        _ => {
            p.expected("`fn`, `type`, `const`, `interface` or `test`");
            ERROR
        }
    };
    if let Some(range) = exposed {
        exposed_error(p, kind, range);
    }
    m.complete(p, kind);
    true
}

/// E0022 for `exposed` on anything but a type declaration (D-150).
/// `tree-sitter-cheby` rejects these; `exposed` without `pub` on a type is
/// left to validation.
fn exposed_error(p: &mut Parser<'_>, kind: SyntaxKind, range: TextRange) {
    let what = match kind {
        TYPE_DECL | ERROR => return,
        ALIAS_DECL => "a type alias",
        FN_DECL => "a function",
        CONST_DECL => "a constant",
        INTERFACE_DECL => "an interface",
        _ => "a test",
    };
    let diag = Diagnostic::new(
        codes::INVALID_EXPOSED,
        format!("`exposed` is not allowed on {what}"),
    )
    .with_primary(p.span(range), "")
    .with_help("`exposed` applies only to `pub` type declarations; remove it");
    p.report(diag);
}

/// `pub` or `priv` (D-162).
pub(super) fn visibility(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump_any();
    m.complete(p, VISIBILITY);
}

/// A function after its visibility: `fn name<T>(params) -> T { … }`. Also
/// used for local functions, which `tree-sitter-cheby` parses the same way.
pub(super) fn fn_rest(p: &mut Parser<'_>) {
    p.bump(FN_KW);
    name(p, LOWER_IDENT, "a function name");
    if p.at(LT) {
        type_params(p);
    }
    if p.at(L_PAREN) {
        param_list(p);
    } else {
        p.expected("`(`");
        return;
    }
    if p.at(THIN_ARROW) {
        ret_type(p);
    }
    if p.at(L_BRACE) {
        block(p);
    }
}

/// `<A, B: Show + Eq>`. Bounds are parsed everywhere; validation rejects
/// them on type declarations (E0023).
pub(super) fn type_params(p: &mut Parser<'_>) {
    let list = List {
        open: LT,
        kind: TYPE_PARAM_LIST,
        first: TokenSet::new(&[UPPER_IDENT]),
        what: "a type parameter",
        non_empty: true,
    };
    comma_list(p, &list, |p| {
        let m = p.start();
        p.bump(UPPER_IDENT);
        if p.eat(COLON) {
            bound_list(p);
        }
        m.complete(p, TYPE_PARAM);
    });
}

/// `(pattern: Type, …)`. Patterns and missing types are accepted in every
/// function; validation rejects them at the top level (E0020, E0021).
pub(super) fn param_list(p: &mut Parser<'_>) {
    let list = List {
        open: L_PAREN,
        kind: PARAM_LIST,
        first: PAT_FIRST,
        what: "a parameter",
        non_empty: false,
    };
    comma_list(p, &list, |p| {
        let m = p.start();
        pattern(p);
        if p.eat(COLON) {
            type_(p);
        }
        m.complete(p, PARAM);
    });
}

/// `-> Type`.
pub(super) fn ret_type(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(THIN_ARROW);
    type_(p);
    m.complete(p, RET_TYPE);
}

/// A type declaration or alias after `exposed`. Decided after the name and
/// parameters: `=` makes an alias, `{` a body, anything else an external
/// type without a body.
fn type_rest(p: &mut Parser<'_>) -> SyntaxKind {
    p.bump(TYPE_KW);
    name(p, UPPER_IDENT, "a type name");
    if p.at(LT) {
        type_params(p);
    }
    if p.eat(EQ) {
        type_(p);
        return ALIAS_DECL;
    }
    if p.at(L_BRACE) {
        // After the doc comments, a type name starts the variants and
        // anything else the record shorthand (App. A.2 notes).
        if p.nth_at(1, UPPER_IDENT) {
            variant_list(p);
        } else {
            record_field_list(p);
        }
    }
    TYPE_DECL
}

fn variant_list(p: &mut Parser<'_>) {
    let list = List {
        open: L_BRACE,
        kind: VARIANT_LIST,
        first: TokenSet::new(&[UPPER_IDENT]),
        what: "a variant",
        non_empty: true,
    };
    comma_list(p, &list, |p| {
        let m = p.start();
        p.bump(UPPER_IDENT);
        if p.at(L_PAREN) {
            let list = List {
                open: L_PAREN,
                kind: POS_FIELD_LIST,
                first: TYPE_FIRST,
                what: "a type",
                non_empty: false,
            };
            comma_list(p, &list, |p| {
                type_(p);
            });
        } else if p.at(L_BRACE) {
            record_field_list(p);
        }
        m.complete(p, VARIANT);
    });
}

/// `{ name: Type, … }`, the fields of a record type or a variant. An empty
/// list is accepted here; `type T {}` is rejected by validation.
fn record_field_list(p: &mut Parser<'_>) {
    let list = List {
        open: L_BRACE,
        kind: RECORD_FIELD_LIST,
        first: TokenSet::new(&[LOWER_IDENT]),
        what: "a field",
        non_empty: false,
    };
    comma_list(p, &list, |p| {
        let m = p.start();
        p.bump(LOWER_IDENT);
        if p.expect(COLON) || p.at_ts(TYPE_FIRST) {
            type_(p);
        }
        m.complete(p, RECORD_FIELD);
    });
}

/// `const name: Type = expr`. A missing type is left to validation (E0020).
fn const_rest(p: &mut Parser<'_>) {
    p.bump(CONST_KW);
    name(p, LOWER_IDENT, "a constant name");
    if p.eat(COLON) {
        type_(p);
    }
    if p.expect(EQ) {
        expr(p);
    }
}

/// `interface Name: Bounds { fn … }`.
fn interface_rest(p: &mut Parser<'_>) {
    p.bump(INTERFACE_KW);
    name(p, UPPER_IDENT, "an interface name");
    if p.eat(COLON) {
        bound_list(p);
    }
    if p.at(L_BRACE) {
        // D-427: functions separated by line breaks.
        line_body(
            p,
            INTERFACE_BODY,
            TokenSet::new(&[FN_KW]),
            "an interface function",
            interface_fn,
        );
    }
}

/// `fn name(Type, …) -> Type` in an interface body.
fn interface_fn(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(FN_KW);
    name(p, LOWER_IDENT, "a function name");
    if p.at(L_PAREN) {
        type_list(p);
    } else {
        p.expected("`(`");
    }
    if p.at(THIN_ARROW) {
        ret_type(p);
    }
    m.complete(p, INTERFACE_FN);
}

/// `test "name" { … }`.
fn test_rest(p: &mut Parser<'_>) {
    p.bump(TEST_KW);
    if p.at(STRING_START) {
        string_lit(p);
    } else {
        p.expected("a test name string");
        if p.at(RAW_STRING) {
            p.bump(RAW_STRING);
        }
    }
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.expected("`{`");
    }
}

/// `import a::b`, `import a::b as c`, `import a::b::{X, y as z}` (App. A.1).
/// A list together with an alias is left to validation.
pub(super) fn import_decl(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(IMPORT_KW);
    let path = p.start();
    name(p, LOWER_IDENT, "a module name");
    while p.at(COLON2) && p.nth_at(1, LOWER_IDENT) {
        p.bump(COLON2);
        p.bump(LOWER_IDENT);
    }
    path.complete(p, MODULE_PATH);
    if p.at(COLON2) {
        p.bump(COLON2);
        if p.at(L_BRACE) {
            import_list(p);
        } else {
            p.expected("a module name or `{`");
        }
    }
    if p.at(AS_KW) {
        rename(p, false);
    }
    m.complete(p, IMPORT_DECL);
}

fn import_list(p: &mut Parser<'_>) {
    let list = List {
        open: L_BRACE,
        kind: IMPORT_LIST,
        first: TokenSet::new(&[LOWER_IDENT, UPPER_IDENT]),
        what: "a name to import",
        non_empty: true,
    };
    comma_list(p, &list, |p| {
        let m = p.start();
        p.bump_any();
        if p.at(AS_KW) {
            rename(p, true);
        }
        m.complete(p, IMPORT_ITEM);
    });
}

/// `as name`. A module alias is a `LOWER` name, as in `tree-sitter-cheby`;
/// an imported item's alias may be either case, and a case that differs
/// from the item's is left to validation.
fn rename(p: &mut Parser<'_>, upper: bool) {
    let m = p.start();
    p.bump(AS_KW);
    if upper && p.at(UPPER_IDENT) {
        p.bump(UPPER_IDENT);
    } else {
        name(p, LOWER_IDENT, "a name");
    }
    m.complete(p, RENAME);
}
