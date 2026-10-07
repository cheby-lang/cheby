//! Types, type arguments and bounds (App. A.3).
//!
//! In a type, `<` after a type name always opens type arguments, whose line
//! breaks are ignored (§2.8). A closing `>` may be the first half of `>>`
//! or `>=`, which the input splits (§2.6, D-430).

use super::{List, comma_list, path};
use crate::SyntaxKind::{
    BOUND_LIST, COLON2, DYN_KW, DYN_TYPE, FN_KW, FN_TYPE, INTERFACE_REF, L_PAREN, LOWER_IDENT, LT,
    PATH_TYPE, PLUS, THIN_ARROW, TUPLE_TYPE, TYPE_ARG_LIST, TYPE_LIST, UPPER_IDENT,
};
use crate::parser::recovery::{EXPR_RECOVERY, TYPE_FIRST};
use crate::parser::{CompletedMarker, Parser};

/// A type. Reports and returns `None` if there is none here.
pub(super) fn type_(p: &mut Parser<'_>) -> Option<CompletedMarker> {
    if !p.at_ts(TYPE_FIRST) {
        p.expected("a type");
        if !p.at_ts(EXPR_RECOVERY) && !p.at_item_recovery() {
            p.bump_error();
        }
        return None;
    }
    if !p.enter() {
        return None;
    }
    let done = match p.current() {
        L_PAREN => {
            // Any number of elements; fewer than two is E0030 (V).
            let list = List {
                open: L_PAREN,
                kind: TUPLE_TYPE,
                first: TYPE_FIRST,
                what: "a type",
                non_empty: false,
            };
            comma_list(p, &list, |p| {
                type_(p);
            })
        }
        FN_KW => {
            let m = p.start();
            p.bump(FN_KW);
            if p.at(L_PAREN) {
                type_list(p);
            } else {
                p.expected("`(`");
            }
            if p.at(THIN_ARROW) {
                super::items::ret_type(p);
            }
            m.complete(p, FN_TYPE)
        }
        DYN_KW => {
            let m = p.start();
            p.bump(DYN_KW);
            interface_ref(p);
            m.complete(p, DYN_TYPE)
        }
        _ => path_type(p),
    };
    p.leave();
    Some(done)
}

/// `[module::]Name[<T, …>]`. `Self` is a path type too.
fn path_type(p: &mut Parser<'_>) -> CompletedMarker {
    let m = p.start();
    if p.at(LOWER_IDENT) && !(p.nth_at(1, COLON2) && p.nth_at(2, UPPER_IDENT)) {
        p.expected("a type");
    }
    path(p, false);
    if p.at(LT) {
        type_arg_list(p);
    }
    m.complete(p, PATH_TYPE)
}

/// `<T, …>` after a type name or a turbofish `::`.
pub(super) fn type_arg_list(p: &mut Parser<'_>) {
    let list = List {
        open: LT,
        kind: TYPE_ARG_LIST,
        first: TYPE_FIRST,
        what: "a type",
        non_empty: true,
    };
    comma_list(p, &list, |p| {
        type_(p);
    });
}

/// `(T, …)`: the parameter types of a function type or interface function.
pub(super) fn type_list(p: &mut Parser<'_>) {
    let list = List {
        open: L_PAREN,
        kind: TYPE_LIST,
        first: TYPE_FIRST,
        what: "a type",
        non_empty: false,
    };
    comma_list(p, &list, |p| {
        type_(p);
    });
}

/// `Show + ops::Eq`. The `+` is not a binary operator, so a line break
/// after it ends the line (§2.8).
pub(super) fn bound_list(p: &mut Parser<'_>) {
    let m = p.start();
    interface_ref(p);
    while p.at(PLUS) {
        p.bump(PLUS);
        interface_ref(p);
    }
    m.complete(p, BOUND_LIST);
}

/// `[module::]Interface`. Type arguments are parsed, and rejected by
/// validation (E0023).
fn interface_ref(p: &mut Parser<'_>) {
    if !matches!(p.current(), LOWER_IDENT | UPPER_IDENT) {
        p.expected("an interface name");
        return;
    }
    let m = p.start();
    if p.at(LOWER_IDENT) && !(p.nth_at(1, COLON2) && p.nth_at(2, UPPER_IDENT)) {
        p.expected("an interface name");
    }
    path(p, false);
    if p.at(LT) {
        type_arg_list(p);
    }
    m.complete(p, INTERFACE_REF);
}
