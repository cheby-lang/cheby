//! Checks of patterns: tuple patterns (E0030, D-076), rests (E0032,
//! D-158), float and interpolated literals (E0033, D-113; E0034, §6.3),
//! negative literals with a space after the `-` (E0056, §5.3.1) and the
//! shapes App. A.6 does not have (E0013).

use cheby_diag::{Diagnostic, codes};

use super::has_error_child;
use crate::ast::{
    AsPat, AstNode, FieldPatEntry, FieldPatList, ListPat, LiteralPat, LiteralPatKind, OrPat, Pat,
    PosPatList, TuplePat,
};
use crate::{SyntaxElement, SyntaxKind, SyntaxToken};

/// E0030: `()` and `(p,)` are not tuple patterns. `(p)` is a
/// parenthesized pattern.
pub(super) fn tuple_pat(tuple: TuplePat<'_>, out: &mut Vec<Diagnostic>) {
    let count = tuple.elements().count();
    let has_comma = tuple
        .syntax()
        .first_token_of_kind(SyntaxKind::COMMA)
        .is_some();
    let (message, help) = match count {
        0 if !has_comma => (
            "`()` is not a pattern",
            "the value with no information is `Nil`; match it with `Nil`",
        ),
        1 => (
            "a tuple pattern has at least two elements",
            "write the pattern alone, or `(p)` without the comma",
        ),
        _ => return,
    };
    out.push(
        Diagnostic::new(codes::SHORT_TUPLE, message)
            .with_primary(tuple.significant_span(), "")
            .with_help(help),
    );
}

/// E0032: at most one rest in a list pattern (D-158).
pub(super) fn list_pat(list: ListPat<'_>, out: &mut Vec<Diagnostic>) {
    let mut rests = list.rests();
    let Some(first) = rests.next() else {
        return;
    };
    for extra in rests {
        out.push(
            Diagnostic::new(codes::MISPLACED_REST, "a list pattern has at most one rest")
                .with_primary(extra.significant_span(), "")
                .with_secondary(first.significant_span(), "first rest here")
                .with_help("keep one `..`; fixed elements may stand on both sides of it (D-158)"),
        );
    }
}

/// E0032: `..` comes last and once in a field pattern list (App. A.6,
/// §6.4). A name after it is a grammar error, reported by the parser.
pub(super) fn field_pat_list(list: FieldPatList<'_>, out: &mut Vec<Diagnostic>) {
    let entries: Vec<FieldPatEntry<'_>> = list.entries().collect();
    let mut seen_rest = None;
    for (index, entry) in entries.iter().enumerate() {
        let FieldPatEntry::Rest(rest) = *entry else {
            continue;
        };
        let field_follows = entries
            .iter()
            .skip(index.saturating_add(1))
            .any(|entry| matches!(entry, FieldPatEntry::Field(_)));
        if let Some(first) = seen_rest {
            out.push(
                Diagnostic::new(
                    codes::MISPLACED_REST,
                    "a field pattern list has at most one `..`",
                )
                .with_primary(rest.significant_span(), "")
                .with_secondary(first, "first `..` here")
                .with_help("remove the extra `..`"),
            );
        } else if field_follows {
            out.push(
                Diagnostic::new(
                    codes::MISPLACED_REST,
                    "`..` must come last in a field pattern list",
                )
                .with_primary(rest.significant_span(), "")
                .with_help("move `..` after the fields"),
            );
        }
        seen_rest.get_or_insert(rest.significant_span());
    }
}

/// E0013: a constructor pattern's positional list is not empty
/// (App. A.6, `ctor_pattern`).
pub(super) fn pos_pat_list(list: PosPatList<'_>, out: &mut Vec<Diagnostic>) {
    if list.patterns().next().is_some() || has_error_child(list.syntax()) {
        return;
    }
    out.push(
        Diagnostic::new(
            codes::SYNTAX_ERROR,
            "a constructor pattern's field list is empty",
        )
        .with_primary(list.significant_span(), "")
        .with_suggestion(
            "for a constructor without fields, remove the parentheses",
            list.significant_span(),
            "",
        ),
    );
}

/// E0033, E0034 and E0056 on literal patterns.
pub(super) fn literal_pat(pat: LiteralPat<'_>, out: &mut Vec<Diagnostic>) {
    if let Some(gap) = pat.minus_gap() {
        super::spaced_negative_literal(pat.syntax(), gap, out);
    }
    match pat.kind() {
        Some(LiteralPatKind::Float(_)) => out.push(
            Diagnostic::new(codes::FLOAT_PATTERN, "float literal in a pattern")
                .with_primary(pat.significant_span(), "")
                .with_help(
                    "compare in a guard, such as `x when x == 0.5 => …`, or in a subjectless \
                     `case` (D-113)",
                ),
        ),
        Some(LiteralPatKind::String(string)) => {
            for interpolation in string.interpolations() {
                out.push(
                    Diagnostic::new(
                        codes::INTERPOLATED_PATTERN,
                        "interpolation in a string pattern",
                    )
                    .with_primary(interpolation.significant_span(), "")
                    .with_help("a string pattern matches fixed text; compare with `==` in a guard"),
                );
            }
        }
        _ => {}
    }
}

/// E0013: `as` inside an alternative must be parenthesized: `A as x | B`
/// (App. A.6, `pattern`).
pub(super) fn or_pat(pat: OrPat<'_>, out: &mut Vec<Diagnostic>) {
    let Some(Pat::As(first)) = pat.alternatives().next() else {
        return;
    };
    out.push(
        Diagnostic::new(
            codes::SYNTAX_ERROR,
            "`as` in an alternative must be in parentheses",
        )
        .with_primary(first.significant_span(), "")
        .with_help(
            "write `(A as x) | B`, or put `as` after the last alternative to bind the whole \
             pattern",
        ),
    );
}

/// E0013: one `as` per pattern: `p as a as b` (App. A.6, `pattern`).
pub(super) fn as_pat(pat: AsPat<'_>, out: &mut Vec<Diagnostic>) {
    let Some(Pat::As(_)) = pat.pattern() else {
        return;
    };
    let span = pat
        .syntax()
        .children_with_tokens()
        .filter_map(SyntaxElement::as_token)
        .filter(|token| token.kind() == SyntaxKind::AS_KW)
        .last()
        .map_or_else(|| pat.significant_span(), SyntaxToken::span);
    out.push(
        Diagnostic::new(codes::SYNTAX_ERROR, "a pattern has at most one `as` name")
            .with_primary(span, "")
            .with_help("bind the value once; use the first name"),
    );
}
