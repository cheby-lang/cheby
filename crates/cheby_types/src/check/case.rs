//! `case` expressions (§5.9, C5): subjects, arms, guards and the
//! subjectless form. Exhaustiveness and reachability are checked on THIR
//! by `crate::exhaust`, so nothing is recorded for them here.

use cheby_diag::{Diagnostic, Span, codes};
use cheby_iface::{Ice, Ty};
use cheby_syntax::ast::{self, AstNode, CaseArmKind, Expr};

use super::record::plural;
use super::{BodyCx, Expected, PatSite};
use crate::infer::InferTy;
use crate::thir::{self, ExprId, ExprKind};

/// The type of a `case` as its arms are checked (§5.9.1, §3.12.5).
/// Without a known expected type, the first arm that is not `Never`
/// fixes the type and later arms are checked against it; a `case` whose
/// arms all diverge has type `Never` (D-497).
enum ArmTy {
    /// The context's expected type: every arm is checked against it.
    Expected(Expected),
    /// No expected type, and every arm so far has type `Never`.
    Open,
    /// No expected type: the first arm that is not `Never` fixed it, and
    /// later arms are checked against it.
    Fixed(Expected),
}

impl BodyCx<'_> {
    /// Checks a `case` with or without subjects, against `expected` if
    /// given (the result is already coerced to it, like
    /// [`BodyCx::check_expr`]), and returns its
    /// [`crate::thir::ExprKind::Case`].
    pub fn check_case(
        &mut self,
        case: ast::CaseExpr<'_>,
        expected: Option<&Expected>,
    ) -> Result<ExprId, Ice> {
        let span = case.significant_span();
        // Subjects left to right, without an expected type (§5.9.1).
        let mut subjects = Vec::new();
        for subject in case.subjects() {
            subjects.push(self.check_expr(subject, None)?);
        }
        let subjectless = case.is_subjectless();
        // An expected type still unknown is treated like none, and the
        // `case`'s type is unified with it at the end: a diverging arm
        // checked against it would make it `Never`.
        let mut arm_ty = match expected {
            Some(e) if !matches!(self.infcx.shallow(&e.ty)?, InferTy::Var(_)) => {
                ArmTy::Expected(e.clone())
            }
            _ => ArmTy::Open,
        };
        let against_expected = matches!(arm_ty, ArmTy::Expected(_));
        let mut arms = Vec::new();
        for arm in case.arms() {
            let checked = match arm {
                CaseArmKind::Case(arm) if !subjectless => {
                    self.case_arm(arm, &subjects, &mut arm_ty)?
                }
                CaseArmKind::Cond(arm) if subjectless => self.cond_arm(arm, &mut arm_ty)?,
                // An arm of the other form is a syntax error.
                _ => {
                    self.errored = true;
                    continue;
                }
            };
            arms.push(checked);
        }
        let ty = match arm_ty {
            ArmTy::Expected(e) | ArmTy::Fixed(e) => e.ty,
            // Every arm diverges (or there is none, a syntax error).
            ArmTy::Open => InferTy::NEVER,
        };
        let id = self.push(
            ExprKind::Case {
                subjects: subjects.into(),
                arms: arms.into(),
            },
            ty,
            span,
        )?;
        if against_expected {
            // The arms were checked against it.
            return Ok(id);
        }
        self.expect_expr(id, expected)
    }

    /// `p1, p2 when guard => body`.
    fn case_arm(
        &mut self,
        arm: ast::CaseArm<'_>,
        subjects: &[ExprId],
        arm_ty: &mut ArmTy,
    ) -> Result<thir::Arm, Ice> {
        let span = arm.significant_span();
        let written: Vec<ast::Pat<'_>> = arm.patterns().collect();
        // One pattern per subject (D-052).
        if written.len() != subjects.len() {
            let pats_span = match (written.first(), written.last()) {
                (Some(first), Some(last)) => {
                    cover(first.significant_span(), last.significant_span())
                }
                _ => span,
            };
            self.report(
                Diagnostic::new(
                    codes::WRONG_ARGUMENT_COUNT,
                    format!(
                        "this arm has {} pattern{}, but the `case` has {} subject{}",
                        written.len(),
                        plural(written.len()),
                        subjects.len(),
                        plural(subjects.len())
                    ),
                )
                .with_primary(
                    pats_span,
                    format!(
                        "expected {} pattern{}",
                        subjects.len(),
                        plural(subjects.len())
                    ),
                )
                .with_note("each arm has one pattern per subject, separated by commas (§5.9.2)"),
            );
        }
        let mut pats = Vec::with_capacity(written.len());
        for (n, pat) in written.into_iter().enumerate() {
            let (ty, origin) = match subjects.get(n) {
                Some(&subject) => {
                    let ty = self.ty_of(subject)?;
                    let origin = (
                        self.span_of(subject)?,
                        format!("this subject has type `{}`", self.show(&ty)),
                    );
                    (ty, Some(origin))
                }
                None => (InferTy::ERROR, None),
            };
            pats.push(self.check_pat_from(pat, &ty, PatSite::CaseArm, origin)?);
        }
        let guard = match arm.guard() {
            Some(guard) => self.condition(guard.condition())?,
            None => None,
        };
        let body = self.arm_body(arm.body(), span, arm_ty)?;
        Ok(thir::Arm {
            pats: pats.into(),
            guard,
            body,
            span,
        })
    }

    /// `cond => body` or the final `_ => body` of a subjectless `case`
    /// (§5.9.5, D-109). The `_` arm's placement is checked by syntax
    /// validation (E0035, D-439).
    fn cond_arm(&mut self, arm: ast::CondArm<'_>, arm_ty: &mut ArmTy) -> Result<thir::Arm, Ice> {
        let span = arm.significant_span();
        let guard = if arm.is_default() {
            None
        } else {
            self.condition(arm.condition())?
        };
        let body = self.arm_body(arm.body(), span, arm_ty)?;
        Ok(thir::Arm {
            pats: Box::new([]),
            guard,
            body,
            span,
        })
    }

    /// A guard or a subjectless `case` condition: a `Bool` (§5.9.3).
    /// `None` when it is missing, a syntax error.
    fn condition(&mut self, cond: Option<Expr<'_>>) -> Result<Option<ExprId>, Ice> {
        let Some(cond) = cond else {
            self.errored = true;
            return Ok(None);
        };
        let expected = Expected::new(InferTy::Known(Ty::BOOL), None);
        self.check_expr(cond, Some(&expected)).map(Some)
    }

    /// An arm's body, against the type of the `case` so far (§5.9.1): an
    /// arm of type `Never` takes the other arms' type (§3.12.5, D-235).
    fn arm_body(
        &mut self,
        body: Option<Expr<'_>>,
        span: Span,
        arm_ty: &mut ArmTy,
    ) -> Result<ExprId, Ice> {
        let Some(body) = body else {
            return self.error_expr(span);
        };
        match arm_ty {
            ArmTy::Expected(expected) => {
                let expected = expected.clone();
                self.check_expr(body, Some(&expected))
            }
            ArmTy::Fixed(fixed) => {
                let fixed = fixed.clone();
                if matches!(self.infcx.shallow(&fixed.ty)?, InferTy::Var(_)) {
                    // A type still unknown would become `Never` if a
                    // diverging arm were checked against it, so the arm
                    // is checked on its own and then unified.
                    let id = self.check_expr(body, None)?;
                    let found = self.ty_of(id)?;
                    if self.infcx.shallow(&found)? != InferTy::NEVER {
                        let span = self.span_of(id)?;
                        self.coerce(&found, Some(&fixed), span)?;
                    }
                    Ok(id)
                } else {
                    self.check_expr(body, Some(&fixed))
                }
            }
            ArmTy::Open => {
                let id = self.check_expr(body, None)?;
                let found = self.ty_of(id)?;
                if self.infcx.shallow(&found)? != InferTy::NEVER {
                    let origin = (
                        self.span_of(id)?,
                        "the `case` has the type of this first arm".to_owned(),
                    );
                    *arm_ty = ArmTy::Fixed(Expected::new(found, Some(origin)));
                }
                Ok(id)
            }
        }
    }
}

/// The span from the start of `first` to the end of `last`, in one file.
fn cover(first: Span, last: Span) -> Span {
    Span::new(first.file, first.range.cover(last.range))
}

#[cfg(test)]
mod tests;
