//! Checks of `case`: the final `_` of a subjectless `case` (E0035, D-109)
//! and empty bodies (E0013, App. A.5).

use cheby_diag::{Diagnostic, Edit, Span, TextRange, TextSize, codes};

use super::has_error_child;
use crate::ast::{AstNode, CaseArmKind, CaseExpr, CondArm};

/// E0013 for an empty body, E0035 for the arms of a subjectless `case`.
pub(super) fn case_expr(case: CaseExpr<'_>, out: &mut Vec<Diagnostic>) {
    let Some(body) = case.body() else {
        return;
    };
    let arms: Vec<CaseArmKind<'_>> = body.arms().collect();
    if arms.is_empty() {
        if !has_error_child(body.syntax()) {
            out.push(
                Diagnostic::new(codes::SYNTAX_ERROR, "a `case` needs at least one arm")
                    .with_primary(body.significant_span(), ""),
            );
        }
        return;
    }
    if !case.is_subjectless() {
        return;
    }
    let cond_arms: Vec<CondArm<'_>> = arms
        .iter()
        .filter_map(|arm| match arm {
            CaseArmKind::Cond(arm) => Some(*arm),
            CaseArmKind::Case(_) => None,
        })
        .collect();
    let Some((last, rest)) = cond_arms.split_last() else {
        return;
    };
    // An earlier `_` already says to move it last, so a missing final `_`
    // is not reported on top of it (one error for one mistake).
    let mut misplaced = false;
    for arm in rest {
        if let Some(underscore) = arm.default_token() {
            misplaced = true;
            out.push(
                Diagnostic::new(
                    codes::SUBJECTLESS_CASE_DEFAULT,
                    "`_` must be the last arm of a subjectless `case`",
                )
                .with_primary(underscore.span(), "")
                .with_help("the arms after `_` can never be taken; remove them or move `_` last"),
            );
        }
    }
    if !last.is_default() && !misplaced {
        let span = last
            .condition()
            .map_or_else(|| last.significant_span(), AstNode::significant_span);
        out.push(
            Diagnostic::new(
                codes::SUBJECTLESS_CASE_DEFAULT,
                "a subjectless `case` must end with a `_` arm",
            )
            .with_primary(span, "")
            .with_suggestion_edits(
                "add a final arm `_ => …` for the remaining cases (D-109)",
                vec![final_arm(*last)],
            ),
        );
    }
}

/// The edit of E0035's help: an arm `_ => todo` on a line of its own after
/// `last`, at `last`'s indentation (`features.md` §6.2). A comment after
/// `last` on its line stays there. In a `case` written on one line the
/// new arm goes before the closing `}`, on a line of its own.
fn final_arm(last: CondArm<'_>) -> Edit {
    let node = last.syntax();
    let text = node.tree().text();
    let range = last.significant_span().range;
    let start = usize::try_from(range.start()).unwrap_or(text.len());
    let end = usize::try_from(range.end()).unwrap_or(text.len());
    let line_start = text
        .get(..start)
        .and_then(|s| s.rfind('\n'))
        .map_or(0, |i| i.saturating_add(1));
    let indent: String = text
        .get(line_start..)
        .unwrap_or_default()
        .chars()
        .take_while(|c| matches!(c, ' ' | '\t'))
        .collect();
    let rest = text.get(end..).unwrap_or_default();
    let line = rest
        .find('\n')
        .map_or(rest, |i| rest.get(..i).unwrap_or_default());
    let line = line.strip_suffix('\r').unwrap_or(line);
    let after = line.trim_start_matches([' ', '\t']);
    let (at, newline) = if after.is_empty() || after.starts_with("//") {
        let crlf = rest
            .get(line.len()..)
            .is_some_and(|r| r.starts_with("\r\n"));
        (
            end.saturating_add(line.len()),
            if crlf { "\r\n" } else { "\n" },
        )
    } else {
        (end, "\n")
    };
    let offset = TextSize::try_from(at).unwrap_or_else(|_| range.end());
    Edit {
        span: Span::new(node.tree().file(), TextRange::empty(offset)),
        replacement: format!("{newline}{indent}_ => todo"),
    }
}
