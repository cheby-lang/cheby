//! Checks of `case`: the final `_` of a subjectless `case` (E0035, D-109)
//! and empty bodies (E0013, App. A.5).

use cheby_diag::{Diagnostic, codes};

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
    for arm in rest {
        if let Some(underscore) = arm.default_token() {
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
    if !last.is_default() {
        let span = last
            .condition()
            .map_or_else(|| last.significant_span(), AstNode::significant_span);
        out.push(
            Diagnostic::new(
                codes::SUBJECTLESS_CASE_DEFAULT,
                "a subjectless `case` must end with a `_` arm",
            )
            .with_primary(span, "")
            .with_help("add a final arm `_ => …` for the remaining cases (D-109)"),
        );
    }
}
