//! Checks of written types: short tuples (E0030, D-076) and the
//! tuple-shape warning (W0001, D-243).

use cheby_diag::{Diagnostic, codes};

use crate::SyntaxKind;
use crate::ast::{AstNode, TupleType, Type};

/// E0030 for `()` and `(T)`/`(T,)`, W0001 for a tuple with a tuple
/// element or four or more elements.
pub(super) fn tuple_type(tuple: TupleType<'_>, out: &mut Vec<Diagnostic>) {
    let count = tuple.elements().count();
    let has_comma = tuple
        .syntax()
        .first_token_of_kind(SyntaxKind::COMMA)
        .is_some();
    let short = match count {
        0 if !has_comma => Some((
            "`()` is not a type",
            "the type with no information is `Nil`",
        )),
        1 => Some((
            "a tuple type has at least two elements",
            "write the element type alone; there are no one-element tuples",
        )),
        _ => None,
    };
    if let Some((message, help)) = short {
        out.push(
            Diagnostic::new(codes::SHORT_TUPLE, message)
                .with_primary(tuple.significant_span(), "")
                .with_help(help),
        );
        return;
    }
    let Some(message) = shape_problem(tuple) else {
        return;
    };
    // One warning per written type: an enclosing tuple that is itself
    // reported covers this one.
    let covered = tuple
        .syntax()
        .ancestors()
        .skip(1)
        .filter_map(TupleType::cast)
        .any(|outer| shape_problem(outer).is_some());
    if covered {
        return;
    }
    out.push(
        Diagnostic::new(codes::TUPLE_SHAPE, message)
            .with_primary(tuple.significant_span(), "")
            .with_help("use a record type with named fields instead (D-243)"),
    );
}

/// What makes a tuple type hide meaning (D-243), if anything.
fn shape_problem(tuple: TupleType<'_>) -> Option<String> {
    if tuple
        .elements()
        .any(|element| matches!(element, Type::Tuple(_)))
    {
        return Some("tuple type with a tuple element".to_owned());
    }
    let count = tuple.elements().count();
    (count >= 4).then(|| format!("tuple type with {count} elements"))
}
