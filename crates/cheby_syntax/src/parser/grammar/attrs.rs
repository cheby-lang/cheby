//! Attributes and inner attributes (App. A.1, A.2, §2.7, §4.8).
//!
//! An attribute's arguments are names or plain strings. Which attributes
//! exist (`@external`, `@target`, `@blocking`, `@deprecated`, …) is checked
//! after parsing; the grammar accepts any name.

use super::exprs::string_lit;
use super::{List, comma_list, name};
use crate::SyntaxKind::{
    AT, AT_BANG, ATTR, ATTR_ARG_LIST, INNER_ATTR, L_PAREN, LOWER_IDENT, STRING_START,
};
use crate::TokenSet;
use crate::parser::Parser;

/// `@!name(args)`, an inner attribute (D-141, D-201).
pub(super) fn inner_attr(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(AT_BANG);
    attr_rest(p);
    m.complete(p, INNER_ATTR);
}

/// `@name(args)`, an attribute of the item that follows.
pub(super) fn attr(p: &mut Parser<'_>) {
    let m = p.start();
    p.bump(AT);
    attr_rest(p);
    m.complete(p, ATTR);
}

fn attr_rest(p: &mut Parser<'_>) {
    name(p, LOWER_IDENT, "an attribute name");
    if p.at(L_PAREN) {
        let list = List {
            open: L_PAREN,
            kind: ATTR_ARG_LIST,
            first: TokenSet::new(&[LOWER_IDENT, STRING_START]),
            what: "an attribute argument",
            non_empty: false,
        };
        comma_list(p, &list, |p| {
            if p.at(STRING_START) {
                string_lit(p);
            } else {
                p.bump_any();
            }
        });
    }
}
